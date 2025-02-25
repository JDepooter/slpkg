mod split_indices;

use failure::Error;
use flate2::read::GzDecoder;
use std::fs::File;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::path::PathBuf;
use std::thread;
use zip::read::ZipFile;
use zip::ZipArchive;

#[derive(Debug, Fail)]
enum UnpackError {
    #[fail(display = "Unable to create a folder for unpacking the package")]
    NoFolderForPackage,
    #[fail(
        display = "The output folder cannot be created because a file with the same name already exists"
    )]
    OutputFolderIsAFile,

    #[fail(display = "Package entries with an absolute path will not be extracted")]
    PackageEntryHasAbsolutePath,
}

fn open_slpk_archive(slpk_file_path: PathBuf) -> Result<ZipArchive<impl Read + Seek>, Error> {
    let file = File::open(slpk_file_path)?;
    let buf_reader = BufReader::new(file);
    Ok(ZipArchive::new(buf_reader)?)
}

fn get_unpack_folder(mut slpk_file_path: PathBuf) -> Result<PathBuf, Error> {
    // Try to extract the file stem. This name will be used as the folder name which
    // the package will be unpacked into. If the package has no file_stem, then
    // it cannot be unpacked. We could come up with some other name to use, but
    // this is a fairly unlikely scenario, so just quitting is ok.
    match slpk_file_path.extension() {
        Some(_) => {
            if let Some(file_stem) = slpk_file_path.file_stem() {
                slpk_file_path.set_file_name(file_stem.to_os_string());
            } else {
                // This probably shouldn't happen. Tough to have a file with an
                // extension but no file stem.
                return Err(Error::from(UnpackError::NoFolderForPackage));
            }
        }
        None => {
            // The file has no extension. This means we cannot use the file basename
            // as the unpacking folder.
            return Err(Error::from(UnpackError::NoFolderForPackage));
        }
    }

    // TODO: Probably the behaviour with respect to existing directories
    // should be configurable.

    if slpk_file_path.exists() {
        if slpk_file_path.is_dir() {
            println!("Deleting folder: {}", slpk_file_path.to_string_lossy());
            std::fs::remove_dir_all(slpk_file_path.clone())?;
        } else if slpk_file_path.is_file() {
            // Don't clobber an existing file with the unpack folder.
            return Err(Error::from(UnpackError::OutputFolderIsAFile));
        }
    }

    std::fs::create_dir(slpk_file_path.clone())?;
    Ok(slpk_file_path)
}

fn create_folder_for_entry(
    mut target_directory: PathBuf,
    zip_entry: &PathBuf,
) -> Result<PathBuf, Error> {
    if let Some(parent_path) = zip_entry.parent() {
        if parent_path.is_absolute() {
            return Err(Error::from(UnpackError::PackageEntryHasAbsolutePath));
        } else {
            target_directory.push(parent_path);
            std::fs::create_dir_all(target_directory.clone())?;
        }
    }

    // Entries which don't have a parent will be extracted into the
    // target directory.
    Ok(target_directory)
}

fn unpack_entry(
    mut archive_entry: ZipFile,
    unpack_folder: PathBuf,
    verbose: bool,
) -> Result<(), Error> {
    let archive_entry_path = archive_entry.sanitized_name();
    let target_folder = create_folder_for_entry(unpack_folder, &archive_entry_path)?;

    if let Some("gz") = archive_entry_path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
    {
        if let Some(non_gzip_name) = archive_entry_path.file_stem() {
            let mut target_file_path = target_folder;
            target_file_path.push(non_gzip_name);

            if verbose {
                println!(
                    "Decompress: {} -> {}",
                    archive_entry.name(),
                    target_file_path.to_string_lossy()
                );
            }

            let mut gz_reader = GzDecoder::new(archive_entry);
            let mut target_file = File::create(target_file_path)?;

            // JSON files are pretty-printed.
            if non_gzip_name
                .to_str()
                .map_or(false, |s| s.ends_with("json"))
            {
                let indentation = jsonformat::Indentation::TwoSpace;
                jsonformat::format_reader_writer(gz_reader, target_file, indentation)?;
            } else {
                std::io::copy(&mut gz_reader, &mut target_file)?;
            }
        }
    } else if let Some(name) = archive_entry_path.file_name() {
        let mut target_file_path = target_folder;
        target_file_path.push(name);

        if verbose {
            println!(
                "Copy: {} -> {}",
                archive_entry.name(),
                target_file_path.to_string_lossy()
            );
        }

        let mut target_file = File::create(target_file_path)?;
        std::io::copy(&mut archive_entry, &mut target_file)?;
    }

    Ok(())
}

pub fn unpack(slpk_file_path: &PathBuf, verbose: bool) -> Result<(), Error> {
    println!("Unpacking archive: {}", slpk_file_path.to_string_lossy());

    let slpk_archive = open_slpk_archive(slpk_file_path.clone())?;
    let unpack_folder = get_unpack_folder(slpk_file_path.clone())?;

    let num_entries = slpk_archive.len();
    let num_cores = num_cpus::get();

    let splits = split_indices::split_indices_into_ranges(num_entries, num_cores);
    let mut threads = Vec::with_capacity(splits.len());

    for (start_entry, end_entry) in splits {
        let slpk_file_path = slpk_file_path.clone();
        let unpack_folder = unpack_folder.clone();
        threads.push(thread::spawn(move || -> Result<usize, Error> {
            let mut slpk_archive = open_slpk_archive(slpk_file_path.clone())?;

            let mut entries_unpacked = 0;
            for entry_idx in start_entry..end_entry {
                let archive_entry = slpk_archive.by_index(entry_idx)?;
                unpack_entry(archive_entry, unpack_folder.clone(), verbose)?;
                entries_unpacked += 1;
            }

            Ok(entries_unpacked)
        }));
    }

    let mut total_entries_unpacked = 0;
    for t in threads {
        let thread_result = t.join();
        match thread_result {
            Ok(Ok(n)) => {
                total_entries_unpacked += n;
            }
            Ok(Err(e)) => {
                eprintln!("{}", e);
                // TODO: Should this return, or wait for other threads to finish?
                return Err(e);
            }
            Err(e) => {
                eprintln!("{:?}", e);
                panic!("Thread panicked!")
            }
        }
    }

    println!("{} files unpacked", total_entries_unpacked);

    Ok(())
}
