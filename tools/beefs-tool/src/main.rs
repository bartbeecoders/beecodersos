//! beefs-tool — create and edit BeeFS disk images on the host.

use beefs::{Block, BlockDevice, FileKind, FileSystem, FsError, Partition, BLOCK_SIZE};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::process::ExitCode;

/// A disk image file used as a block device.
struct ImageFile {
    file: File,
    blocks: u64,
}

impl ImageFile {
    fn open(path: &str) -> std::io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let blocks = file.metadata()?.len() / BLOCK_SIZE as u64;
        Ok(ImageFile { file, blocks })
    }

    fn create(path: &str, size_bytes: u64) -> std::io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(true).open(path)?;
        file.set_len(size_bytes)?;
        Ok(ImageFile { file, blocks: size_bytes / BLOCK_SIZE as u64 })
    }

    fn seek(&mut self, index: u64) -> beefs::Result<()> {
        if index >= self.blocks {
            return Err(FsError::OutOfRange);
        }
        self.file.seek(SeekFrom::Start(index * BLOCK_SIZE as u64)).map_err(|_| FsError::Io)?;
        Ok(())
    }
}

impl BlockDevice for ImageFile {
    fn block_count(&self) -> u64 {
        self.blocks
    }
    fn read_block(&mut self, index: u64, buf: &mut Block) -> beefs::Result<()> {
        self.seek(index)?;
        self.file.read_exact(buf).map_err(|_| FsError::Io)
    }
    fn write_block(&mut self, index: u64, buf: &Block) -> beefs::Result<()> {
        self.seek(index)?;
        self.file.write_all(buf).map_err(|_| FsError::Io)
    }
    fn flush(&mut self) -> beefs::Result<()> {
        self.file.flush().map_err(|_| FsError::Io)
    }
}

const USAGE: &str = "\
beefs-tool — manage BeeFS disk images

usage: beefs-tool <image> <command> [args]

<image> is a BeeFS image, or a boot disk image whose BeeFS partition is used.

commands:
  mkfs <size-MiB>          create/overwrite the image with an empty BeeFS
  info                     show filesystem usage
  ls [path]                list a directory (default /)
  tree [path]              show a directory tree
  mkdir <path>             create a directory (with parents)
  put <host-file> <path>   copy a host file into the image
  get <path> <host-file>   copy a file out of the image
  cat <path>               print a file
  write <path> <text>      write text to a file (replaces it)
  rm <path>                remove a file or empty directory
  mv <from> <to>           move / rename";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("beefs-tool: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let (image, cmd, rest) = match args {
        [image, cmd, rest @ ..] => (image.as_str(), cmd.as_str(), rest),
        _ => return Err(USAGE.into()),
    };
    let arg = |i: usize| rest.get(i).map(String::as_str).ok_or_else(|| USAGE.to_string());
    let fserr = |e: FsError| e.to_string();

    if cmd == "mkfs" {
        let mib: u64 = arg(0)?.parse().map_err(|_| "size must be a number of MiB")?;
        let dev = ImageFile::create(image, mib * 1024 * 1024).map_err(|e| e.to_string())?;
        let fs = FileSystem::format(dev).map_err(fserr)?;
        let s = fs.stats();
        println!("{image}: BeeFS, {mib} MiB, {} blocks, {} inodes", s.total_blocks, s.total_inodes);
        return Ok(());
    }

    let dev = ImageFile::open(image).map_err(|e| format!("{image}: {e}"))?;
    // A boot disk (tools/bootdisk.py) keeps BeeFS in its second partition.
    let (dev, _) = Partition::beefs_or_whole(dev).map_err(|e| format!("{image}: {e}"))?;
    let mut fs = FileSystem::mount(dev).map_err(|e| format!("{image}: {e}"))?;

    match cmd {
        "info" => {
            let s = fs.stats();
            let kib = |b: u32| b as u64 * s.block_size as u64 / 1024;
            println!("block size : {} bytes", s.block_size);
            println!("blocks     : {} total, {} free ({} KiB free)", s.total_blocks, s.free_blocks, kib(s.free_blocks));
            println!("inodes     : {} total, {} free", s.total_inodes, s.free_inodes);
        }
        "ls" => {
            let path = rest.first().map(String::as_str).unwrap_or("/");
            for e in fs.read_dir(path).map_err(fserr)? {
                match e.kind {
                    FileKind::Directory => println!("  <dir> {:>8}  {}/", "", e.name),
                    FileKind::File => println!("        {:>8}  {}", e.size, e.name),
                }
            }
        }
        "tree" => {
            let path = rest.first().map(String::as_str).unwrap_or("/");
            println!("{path}");
            tree(&mut fs, path, 1)?;
        }
        "mkdir" => fs.mkdir_all(arg(0)?).map_err(fserr)?,
        "put" => {
            let data = std::fs::read(arg(0)?).map_err(|e| format!("{}: {e}", arg(0).unwrap()))?;
            fs.write_file(arg(1)?, &data).map_err(fserr)?;
        }
        "get" => {
            let data = fs.read_file(arg(0)?).map_err(fserr)?;
            std::fs::write(arg(1)?, data).map_err(|e| e.to_string())?;
        }
        "cat" => {
            let data = fs.read_file(arg(0)?).map_err(fserr)?;
            std::io::stdout().write_all(&data).map_err(|e| e.to_string())?;
        }
        "write" => fs.write_file(arg(0)?, arg(1)?.as_bytes()).map_err(fserr)?,
        "rm" => fs.remove(arg(0)?).map_err(fserr)?,
        "mv" => fs.rename(arg(0)?, arg(1)?).map_err(fserr)?,
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

fn tree(fs: &mut FileSystem<Partition<ImageFile>>, path: &str, depth: usize) -> Result<(), String> {
    for e in fs.read_dir(path).map_err(|e| e.to_string())? {
        let indent = "  ".repeat(depth);
        let child = format!("{}/{}", path.trim_end_matches('/'), e.name);
        match e.kind {
            FileKind::Directory => {
                println!("{indent}{}/", e.name);
                tree(fs, &child, depth + 1)?;
            }
            FileKind::File => println!("{indent}{} ({} bytes)", e.name, e.size),
        }
    }
    Ok(())
}
