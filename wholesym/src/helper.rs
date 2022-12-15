use debugid::{CodeId, DebugId};
use samply_api::samply_symbols::{self, LibraryInfo};
use samply_symbols::{
    CandidatePathInfo, FileAndPathHelper, FileAndPathHelperResult, FileLocation,
    OptionallySendFuture,
};
use symsrv::{memmap2, FileContents, SymbolCache};

use std::{
    collections::HashMap,
    fs::File,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex},
};

use crate::config::SymbolManagerConfig;

/// A simple helper which only exists to let samply_symbols::SymbolManager open
/// local files for the binary_at_path functions.
pub struct FileReadOnlyHelper;

impl FileReadOnlyHelper {
    async fn open_file_impl(
        &self,
        location: FileLocation,
    ) -> FileAndPathHelperResult<FileContents> {
        match location {
            FileLocation::Path(path) => {
                let file = File::open(&path)?;
                Ok(FileContents::Mmap(unsafe {
                    memmap2::MmapOptions::new().map(&file)?
                }))
            }
            FileLocation::Custom(_) => {
                panic!("FileLocation::Custom should not be hit in FileReadOnlyHelper");
            }
        }
    }
}

impl<'h> FileAndPathHelper<'h> for FileReadOnlyHelper {
    type F = FileContents;
    type OpenFileFuture =
        Pin<Box<dyn OptionallySendFuture<Output = FileAndPathHelperResult<Self::F>> + 'h>>;

    fn get_candidate_paths_for_debug_file(
        &self,
        _library_info: &LibraryInfo,
    ) -> FileAndPathHelperResult<Vec<CandidatePathInfo>> {
        panic!("Should not be called");
    }

    fn get_candidate_paths_for_binary(
        &self,
        _library_info: &LibraryInfo,
    ) -> FileAndPathHelperResult<Vec<CandidatePathInfo>> {
        panic!("Should not be called");
    }

    fn open_file(
        &'h self,
        location: &FileLocation,
    ) -> Pin<Box<dyn OptionallySendFuture<Output = FileAndPathHelperResult<Self::F>> + 'h>> {
        Box::pin(self.open_file_impl(location.clone()))
    }
}

pub struct Helper {
    symbol_cache: Option<SymbolCache>,
    known_libs: Mutex<KnownLibs>,
    config: SymbolManagerConfig,
}

#[derive(Debug, Clone, Default)]
struct KnownLibs {
    by_debug: HashMap<(String, DebugId), Arc<LibraryInfo>>,
    by_code: HashMap<(Option<String>, CodeId), Arc<LibraryInfo>>,
}

impl Helper {
    pub fn with_config(config: SymbolManagerConfig) -> Self {
        let symbol_cache = match config.effective_nt_symbol_path() {
            Some(nt_symbol_path) => Some(SymbolCache::new(nt_symbol_path, config.verbose)),
            None => None,
        };
        Self {
            symbol_cache,
            known_libs: Mutex::new(Default::default()),
            config,
        }
    }

    pub fn add_known_lib(&self, lib_info: LibraryInfo) {
        let mut known_libs = self.known_libs.lock().unwrap();
        let lib_info = Arc::new(lib_info);
        if let (Some(debug_name), Some(debug_id)) = (lib_info.debug_name.clone(), lib_info.debug_id)
        {
            known_libs
                .by_debug
                .insert((debug_name, debug_id), lib_info.clone());
        }
        if let (name, Some(code_id)) = (lib_info.name.clone(), lib_info.code_id.clone()) {
            known_libs.by_code.insert((name, code_id), lib_info.clone());
        }
    }

    async fn open_file_impl(
        &self,
        location: FileLocation,
    ) -> FileAndPathHelperResult<FileContents> {
        match location {
            FileLocation::Path(path) => {
                if self.config.verbose {
                    eprintln!("Opening file {:?}", path.to_string_lossy());
                }
                let file = File::open(&path)?;
                Ok(FileContents::Mmap(unsafe {
                    memmap2::MmapOptions::new().map(&file)?
                }))
            }
            FileLocation::Custom(custom) => {
                if let Some(path) = custom.strip_prefix("winsymbolserver:") {
                    if self.config.verbose {
                        eprintln!("Trying to get file {:?} from symbol cache", path);
                    }
                    Ok(self
                        .symbol_cache
                        .as_ref()
                        .unwrap()
                        .get_file(Path::new(path))
                        .await?)
                } else if let Some(path) = custom.strip_prefix("bpsymbolserver:") {
                    if self.config.verbose {
                        eprintln!("Trying to get file {:?} from breakpad symbol server", path);
                    }
                    self.get_bp_sym_file(path).await
                } else {
                    panic!("Unexpected custom path: {}", custom);
                }
            }
        }
    }

    async fn get_bp_sym_file(&self, rel_path: &str) -> FileAndPathHelperResult<FileContents> {
        for (server_base_url, cache_dir) in &self.config.breakpad_servers {
            if let Ok(file) = self
                .get_bp_sym_file_from_server(rel_path, server_base_url, cache_dir)
                .await
            {
                return Ok(file);
            }
        }
        Err("No breakpad sym file on server".into())
    }

    async fn get_bp_sym_file_from_server(
        &self,
        rel_path: &str,
        server_base_url: &str,
        cache_dir: &Path,
    ) -> FileAndPathHelperResult<FileContents> {
        let url = format!("{}/{}", server_base_url, rel_path);
        if self.config.verbose {
            eprintln!("Downloading {}...", url);
        }
        let sym_file_response = reqwest::get(&url).await?.error_for_status()?;
        let mut stream = sym_file_response.bytes_stream();
        let dest_path = cache_dir.join(rel_path);
        if let Some(dir) = dest_path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        if self.config.verbose {
            eprintln!("Saving bytes to {:?}.", dest_path);
        }
        let file = tokio::fs::File::create(&dest_path).await?;
        let mut writer = tokio::io::BufWriter::new(file);
        use futures_util::StreamExt;
        while let Some(item) = stream.next().await {
            tokio::io::copy(&mut item?.as_ref(), &mut writer).await?;
        }
        drop(writer);
        if self.config.verbose {
            eprintln!("Opening file {:?}", dest_path.to_string_lossy());
        }
        let file = File::open(&dest_path)?;
        Ok(FileContents::Mmap(unsafe {
            memmap2::MmapOptions::new().map(&file)?
        }))
    }

    fn fill_in_library_info_details(&self, info: &mut LibraryInfo) {
        let known_libs = self.known_libs.lock().unwrap();

        // Look up (debugName, breakpadId) in the known libs.
        if let (Some(debug_name), Some(debug_id)) = (&info.debug_name, info.debug_id) {
            if let Some(known_info) = known_libs.by_debug.get(&(debug_name.to_string(), debug_id)) {
                info.absorb(known_info);
            }
        }

        // If all we have is the ELF build ID, maybe we have some paths in the known libs.
        if let Some(code_id) = info.code_id.clone() {
            if let Some(known_info) = known_libs.by_code.get(&(info.name.clone(), code_id)) {
                info.absorb(known_info);
            }
        }
    }
}

impl<'h> FileAndPathHelper<'h> for Helper {
    type F = FileContents;
    type OpenFileFuture =
        Pin<Box<dyn OptionallySendFuture<Output = FileAndPathHelperResult<Self::F>> + 'h>>;

    fn get_candidate_paths_for_debug_file(
        &self,
        library_info: &LibraryInfo,
    ) -> FileAndPathHelperResult<Vec<CandidatePathInfo>> {
        let mut paths = vec![];

        let mut info = library_info.clone();
        self.fill_in_library_info_details(&mut info);

        let mut got_dsym = false;

        if let (Some(debug_path), Some(debug_name)) = (&info.debug_path, &info.debug_name) {
            if let Some(debug_id) = info.debug_id {
                // First, see if we can find a dSYM file for the binary.
                if let Some(dsym_path) =
                    crate::moria_mac::locate_dsym_fastpath(Path::new(debug_path), debug_id.uuid())
                {
                    got_dsym = true;
                    paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                        dsym_path.clone(),
                    )));
                    paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                        dsym_path
                            .join("Contents")
                            .join("Resources")
                            .join("DWARF")
                            .join(debug_name),
                    )));
                }
            }

            // Also consider .so.dbg files in the same directory.
            if debug_path.ends_with(".so") {
                let so_dbg_path = format!("{}.dbg", debug_path);
                paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                    PathBuf::from(so_dbg_path),
                )));
            }

            if debug_path.ends_with(".pdb") {
                // Get symbols from the pdb file.
                paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                    debug_path.into(),
                )));
            }
        }

        if !got_dsym {
            if let Some(debug_id) = info.debug_id {
                // Try a little harder to find a dSYM, just from the UUID. We can do this
                // even if we don't have an entry for this library in the libinfo map.
                if let Ok(dsym_path) =
                    crate::moria_mac::locate_dsym_using_spotlight(debug_id.uuid())
                {
                    paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                        dsym_path.clone(),
                    )));
                    if let Some(dsym_file_name) = dsym_path.file_name().and_then(|s| s.to_str()) {
                        paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                            dsym_path
                                .join("Contents")
                                .join("Resources")
                                .join("DWARF")
                                .join(dsym_file_name.trim_end_matches(".dSYM")),
                        )));
                    }
                }
            }
        }

        // Find debuginfo in /usr/lib/debug/.build-id/ etc.
        // <https://sourceware.org/gdb/onlinedocs/gdb/Separate-Debug-Files.html>
        if let Some(code_id) = &info.code_id {
            let code_id = code_id.as_str();
            if code_id.len() > 2 {
                let (two_chars, rest) = code_id.split_at(2);
                let path = format!("/usr/lib/debug/.build-id/{}/{}.debug", two_chars, rest);
                paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                    PathBuf::from(path),
                )));
            }
        }

        if let Some(debug_name) = &info.debug_name {
            // Fake "debug link" support. We hardcode a "debug link name" of
            // `{debug_name}.debug`.
            // It would be better to get the actual debug link name from the binary.
            paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                PathBuf::from(format!("/usr/bin/{}.debug", &debug_name)),
            )));
            paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                PathBuf::from(format!("/usr/bin/.debug/{}.debug", &debug_name)),
            )));
            paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                PathBuf::from(format!("/usr/lib/debug/usr/bin/{}.debug", &debug_name)),
            )));

            if let Some(debug_id) = info.debug_id {
                // Search breakpad symbol directories.
                for dir in &self.config.breakpad_directories_readonly {
                    let bp_path = dir
                        .join(debug_name)
                        .join(debug_id.breakpad().to_string())
                        .join(&format!("{}.sym", debug_name.trim_end_matches(".pdb")));
                    paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(bp_path)));
                }

                for (_url, dir) in &self.config.breakpad_servers {
                    let bp_path = dir
                        .join(debug_name)
                        .join(debug_id.breakpad().to_string())
                        .join(&format!("{}.sym", debug_name.trim_end_matches(".pdb")));
                    paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(bp_path)));
                }

                if debug_name.ends_with(".pdb") && self.symbol_cache.is_some() {
                    // We might find this pdb file with the help of a symbol server.
                    // Construct a custom string to identify this pdb.
                    let custom = format!(
                        "winsymbolserver:{}/{}/{}",
                        debug_name,
                        debug_id.breakpad(),
                        debug_name
                    );
                    paths.push(CandidatePathInfo::SingleFile(FileLocation::Custom(custom)));
                }

                if !self.config.breakpad_servers.is_empty() {
                    // We might find a .sym file on a symbol server.
                    // Construct a custom string to identify this file.
                    let custom = format!(
                        "bpsymbolserver:{}/{}/{}.sym",
                        debug_name,
                        debug_id.breakpad(),
                        debug_name.trim_end_matches(".pdb")
                    );
                    paths.push(CandidatePathInfo::SingleFile(FileLocation::Custom(custom)));
                }
            }
        }

        if let Some(path) = &info.path {
            // Fall back to getting symbols from the binary itself.
            paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                path.into(),
            )));

            // For macOS system libraries, also consult the dyld shared cache.
            if path.starts_with("/usr/") || path.starts_with("/System/") {
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld/dyld_shared_cache_arm64e")
                        .to_path_buf(),
                    dylib_path: path.clone(),
                });
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld/dyld_shared_cache_x86_64")
                        .to_path_buf(),
                    dylib_path: path.clone(),
                });
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Library/dyld/dyld_shared_cache_arm64e")
                        .to_path_buf(),
                    dylib_path: path.clone(),
                });
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Library/dyld/dyld_shared_cache_x86_64h")
                        .to_path_buf(),
                    dylib_path: path.clone(),
                });
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Library/dyld/dyld_shared_cache_x86_64")
                        .to_path_buf(),
                    dylib_path: path.clone(),
                });
            }
        }

        Ok(paths)
    }

    fn get_candidate_paths_for_binary(
        &self,
        library_info: &LibraryInfo,
    ) -> FileAndPathHelperResult<Vec<CandidatePathInfo>> {
        let mut info = library_info.clone();
        self.fill_in_library_info_details(&mut info);

        let mut paths = vec![];

        // Begin with the binary itself.
        if let Some(path) = &info.path {
            paths.push(CandidatePathInfo::SingleFile(FileLocation::Path(
                path.into(),
            )));
        }

        if let (Some(_symbol_cache), Some(name), Some(code_id)) =
            (&self.symbol_cache, &info.name, &info.code_id)
        {
            // We might find this exe / dll file with the help of a symbol server.
            // Construct a custom string to identify this file.
            // TODO: Adjust case for case-sensitive symbol servers.
            let custom = format!("winsymbolserver:{}/{}/{}", name, code_id, name);
            paths.push(CandidatePathInfo::SingleFile(FileLocation::Custom(custom)));
        }

        if let Some(path) = &info.path {
            // For macOS system libraries, also consult the dyld shared cache.
            if path.starts_with("/usr/") || path.starts_with("/System/") {
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld/dyld_shared_cache_arm64e")
                        .to_path_buf(),
                    dylib_path: path.clone(),
                });
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld/dyld_shared_cache_x86_64")
                        .to_path_buf(),
                    dylib_path: path.clone(),
                });
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Library/dyld/dyld_shared_cache_arm64e")
                        .to_path_buf(),
                    dylib_path: path.clone(),
                });
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Library/dyld/dyld_shared_cache_x86_64h")
                        .to_path_buf(),
                    dylib_path: path.clone(),
                });
                paths.push(CandidatePathInfo::InDyldCache {
                    dyld_cache_path: Path::new("/System/Library/dyld/dyld_shared_cache_x86_64")
                        .to_path_buf(),
                    dylib_path: path.clone(),
                });
            }
        }

        Ok(paths)
    }

    fn open_file(
        &'h self,
        location: &FileLocation,
    ) -> Pin<Box<dyn OptionallySendFuture<Output = FileAndPathHelperResult<Self::F>> + 'h>> {
        Box::pin(self.open_file_impl(location.clone()))
    }
}