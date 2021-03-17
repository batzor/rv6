//! File-system system calls.
//! Mostly argument checking, since we don't trust
//! user code, and calls into file.c and fs.c.

#![allow(clippy::unit_arg)]

use core::{cell::UnsafeCell, mem};

use arrayvec::ArrayVec;
use cstr_core::CStr;

use crate::{
    fcntl::FcntlFlags,
    file::{FileType, InodeFileType, RcFile},
    fs::{Dirent, FileName, FsTransaction, InodeGuard, InodeType, Path, RcInode},
    kernel::Kernel,
    ok_or,
    page::Page,
    param::{MAXARG, MAXPATH, NOFILE},
    proc::CurrentProc,
    some_or,
    syscall::{argaddr, argint, argstr, fetchaddr, fetchstr},
    vm::UVAddr,
};

impl RcFile<'static> {
    /// Allocate a file descriptor for the given file.
    /// Takes over file reference from caller on success.
    fn fdalloc(self, proc: &mut CurrentProc<'_>) -> Result<i32, Self> {
        let proc_data = proc.deref_mut_data();
        for fd in 0..NOFILE {
            // user pointer to struct stat
            if proc_data.open_files[fd].is_none() {
                proc_data.open_files[fd] = Some(self);
                return Ok(fd as i32);
            }
        }
        Err(self)
    }
}

/// Fetch the nth word-sized system call argument as a file descriptor
/// and return both the descriptor and the corresponding struct file.
fn argfd<'a>(n: usize, proc: &'a CurrentProc<'a>) -> Result<(i32, &'a RcFile<'static>), ()> {
    let fd = argint(n, proc)?;
    if fd < 0 || fd >= NOFILE as i32 {
        return Err(());
    }

    let f = some_or!(&proc.deref_data().open_files[fd as usize], return Err(()));

    Ok((fd, f))
}

impl Kernel {
    /// Create an inode with given type.
    /// Returns Ok(created inode, result of given function f) on success, Err(()) on error.
    fn create<F, T>(
        &self,
        path: &Path,
        typ: InodeType,
        tx: &FsTransaction<'_>,
        proc: &CurrentProc<'_>,
        f: F,
    ) -> Result<(RcInode<'_>, T), ()>
    where
        F: FnOnce(&mut InodeGuard<'_>) -> T,
    {
        let (ptr, name) = path.nameiparent(proc, &self.itable)?;
        let mut dp = ptr.lock();
        if let Ok((ptr2, _)) = dp.dirlookup(&name, &self.itable) {
            drop(dp);
            let mut ip = ptr2.lock();
            if typ == InodeType::File {
                match ip.deref_inner().typ {
                    InodeType::File | InodeType::Device { .. } => {
                        let ret = f(&mut ip);
                        drop(ip);
                        return Ok((ptr2, ret));
                    }
                    _ => return Err(()),
                }
            }
            return Err(());
        }
        let ptr2 = self.itable.alloc_inode(dp.dev, typ, tx);
        let mut ip = ptr2.lock();
        ip.deref_inner_mut().nlink = 1;
        ip.update(tx);

        // Create . and .. entries.
        if typ == InodeType::Dir {
            // for ".."
            dp.deref_inner_mut().nlink += 1;
            dp.update(tx);

            // No ip->nlink++ for ".": avoid cyclic ref count.
            // It is safe because b"." does not contain any NUL characters.
            ip.dirlink(
                unsafe { FileName::from_bytes(b".") },
                ip.inum,
                tx,
                &self.itable,
            )
            // It is safe because b".." does not contain any NUL characters.
            .and_then(|_| {
                ip.dirlink(
                    unsafe { FileName::from_bytes(b"..") },
                    dp.inum,
                    tx,
                    &self.itable,
                )
            })
            .expect("create dots");
        }
        dp.dirlink(&name, ip.inum, tx, &self.itable)
            .expect("create: dirlink");
        let ret = f(&mut ip);
        drop(ip);
        Ok((ptr2, ret))
    }

    /// Create another name(newname) for the file oldname.
    /// Returns Ok(()) on success, Err(()) on error.
    fn link(&self, oldname: &CStr, newname: &CStr, proc: &CurrentProc<'_>) -> Result<(), ()> {
        let tx = self.file_system.begin_transaction();
        let ptr = Path::new(oldname).namei(proc, &self.itable)?;
        let mut ip = ptr.lock();
        if ip.deref_inner().typ == InodeType::Dir {
            return Err(());
        }
        ip.deref_inner_mut().nlink += 1;
        ip.update(&tx);
        drop(ip);

        if let Ok((ptr2, name)) = Path::new(newname).nameiparent(proc, &self.itable) {
            let mut dp = ptr2.lock();
            if dp.dev != ptr.dev || dp.dirlink(name, ptr.inum, &tx, &self.itable).is_err() {
            } else {
                return Ok(());
            }
        }

        let mut ip = ptr.lock();
        ip.deref_inner_mut().nlink -= 1;
        ip.update(&tx);
        Err(())
    }

    /// Remove a file(filename).
    /// Returns Ok(()) on success, Err(()) on error.
    fn unlink(&self, filename: &CStr, proc: &CurrentProc<'_>) -> Result<(), ()> {
        let de: Dirent = Default::default();
        let tx = self.file_system.begin_transaction();
        let (ptr, name) = Path::new(filename).nameiparent(proc, &self.itable)?;
        let mut dp = ptr.lock();

        // Cannot unlink "." or "..".
        if !(name.as_bytes() == b"." || name.as_bytes() == b"..") {
            if let Ok((ptr2, off)) = dp.dirlookup(&name, &self.itable) {
                let mut ip = ptr2.lock();
                assert!(ip.deref_inner().nlink >= 1, "unlink: nlink < 1");

                if ip.deref_inner().typ != InodeType::Dir || ip.is_dir_empty() {
                    dp.write_kernel(&de, off, &tx).expect("unlink: writei");
                    if ip.deref_inner().typ == InodeType::Dir {
                        dp.deref_inner_mut().nlink -= 1;
                        dp.update(&tx);
                    }
                    drop(dp);
                    drop(ptr);
                    ip.deref_inner_mut().nlink -= 1;
                    ip.update(&tx);
                    return Ok(());
                }
            }
        }

        Err(())
    }

    /// Open a file; omode indicate read/write.
    /// Returns Ok(file descriptor) on success, Err(()) on error.
    fn open(
        &'static self,
        name: &Path,
        omode: FcntlFlags,
        proc: &mut CurrentProc<'_>,
    ) -> Result<usize, ()> {
        let tx = self.file_system.begin_transaction();

        let (ip, typ) = if omode.contains(FcntlFlags::O_CREATE) {
            self.create(name, InodeType::File, &tx, proc, |ip| ip.deref_inner().typ)?
        } else {
            let ptr = name.namei(proc, &self.itable)?;
            let ip = ptr.lock();
            let typ = ip.deref_inner().typ;

            if typ == InodeType::Dir && omode != FcntlFlags::O_RDONLY {
                return Err(());
            }
            drop(ip);
            (ptr, typ)
        };

        let filetype = match typ {
            InodeType::Device { major, .. } => {
                let major = self.devsw.get(major as usize).ok_or(())?;
                FileType::Device { ip, major }
            }
            _ => {
                FileType::Inode {
                    inner: InodeFileType {
                        ip,
                        off: UnsafeCell::new(0),
                    },
                }
            }
        };

        let f = self.ftable.alloc_file(
            filetype,
            !omode.intersects(FcntlFlags::O_WRONLY),
            omode.intersects(FcntlFlags::O_WRONLY | FcntlFlags::O_RDWR),
        )?;

        if omode.contains(FcntlFlags::O_TRUNC) && typ == InodeType::File {
            match &f.typ {
                // It is safe to call itrunc because ip.lock() is held
                FileType::Device { ip, .. }
                | FileType::Inode {
                    inner: InodeFileType { ip, .. },
                } => ip.lock().itrunc(&tx),
                _ => panic!("sys_open : Not reach"),
            };
        }
        let fd = f.fdalloc(proc).map_err(|_| ())?;
        Ok(fd as usize)
    }

    /// Create a new directory.
    /// Returns Ok(()) on success, Err(()) on error.
    fn mkdir(&self, dirname: &CStr, proc: &CurrentProc<'_>) -> Result<(), ()> {
        let tx = self.file_system.begin_transaction();
        self.create(Path::new(dirname), InodeType::Dir, &tx, proc, |_| ())?;
        Ok(())
    }

    /// Create a device file.
    /// Returns Ok(()) on success, Err(()) on error.
    fn mknod(
        &self,
        filename: &CStr,
        major: u16,
        minor: u16,
        proc: &CurrentProc<'_>,
    ) -> Result<(), ()> {
        let tx = self.file_system.begin_transaction();
        self.create(
            Path::new(filename),
            InodeType::Device { major, minor },
            &tx,
            proc,
            |_| (),
        )?;
        Ok(())
    }

    /// Change the current directory.
    /// Returns Ok(()) on success, Err(()) on error.
    fn chdir(&'static self, dirname: &CStr, proc: &mut CurrentProc<'_>) -> Result<(), ()> {
        // TODO(https://github.com/kaist-cp/rv6/issues/290)
        // The method namei can drop inodes. If namei succeeds, its return
        // value, ptr, will be dropped when this method returns. Deallocation
        // of an inode may cause disk write operations, so we must begin a
        // transaction here.
        let _tx = self.file_system.begin_transaction();
        let ptr = Path::new(dirname).namei(proc, &self.itable)?;
        let ip = ptr.lock();
        if ip.deref_inner().typ != InodeType::Dir {
            return Err(());
        }
        drop(ip);
        let _ = mem::replace(proc.cwd_mut(), ptr);
        Ok(())
    }

    /// Create a pipe, put read/write file descriptors in fd0 and fd1.
    /// Returns Ok(()) on success, Err(()) on error.
    fn pipe(&'static self, fdarray: UVAddr, proc: &mut CurrentProc<'_>) -> Result<(), ()> {
        let (pipereader, pipewriter) = self.allocate_pipe()?;

        let fd0 = pipereader.fdalloc(proc).map_err(|_| ())?;
        let fd1 = pipewriter
            .fdalloc(proc)
            .map_err(|_| proc.deref_mut_data().open_files[fd0 as usize] = None)?;

        if proc.memory_mut().copy_out(fdarray, &fd0).is_err()
            || proc
                .memory_mut()
                .copy_out(fdarray + mem::size_of::<i32>(), &fd1)
                .is_err()
        {
            let proc_data = proc.deref_mut_data();
            proc_data.open_files[fd0 as usize] = None;
            proc_data.open_files[fd1 as usize] = None;
            return Err(());
        }
        Ok(())
    }
}

impl Kernel {
    /// Return a new file descriptor referring to the same file as given fd.
    /// Returns Ok(new file descriptor) on success, Err(()) on error.
    pub fn sys_dup(&self, proc: &mut CurrentProc<'_>) -> Result<usize, ()> {
        let (_, f) = argfd(0, proc)?;
        let newfile = f.clone();
        let fd = newfile.fdalloc(proc).map_err(|_| ())?;
        Ok(fd as usize)
    }

    /// Read n bytes into buf.
    /// Returns Ok(number read) on success, Err(()) on error.
    pub fn sys_read(&self, proc: &mut CurrentProc<'_>) -> Result<usize, ()> {
        let (_, f) = argfd(0, proc)?;
        let n = argint(2, proc)?;
        let p = argaddr(1, proc)?;
        // Safe since read will not access proc's open_files.
        unsafe { (*(f as *const RcFile<'static>)).read(p.into(), n, proc) }
    }

    /// Write n bytes from buf to given file descriptor fd.
    /// Returns Ok(n) on success, Err(()) on error.
    pub fn sys_write(&self, proc: &mut CurrentProc<'_>) -> Result<usize, ()> {
        let (_, f) = argfd(0, proc)?;
        let n = argint(2, proc)?;
        let p = argaddr(1, proc)?;
        // Safe since write will not access proc's open_files.
        unsafe { (*(f as *const RcFile<'static>)).write(p.into(), n, proc, &self.file_system) }
    }

    /// Release open file fd.
    /// Returns Ok(0) on success, Err(()) on error.
    pub fn sys_close(&self, proc: &mut CurrentProc<'_>) -> Result<usize, ()> {
        let (fd, _) = argfd(0, proc)?;
        proc.deref_mut_data().open_files[fd as usize] = None;
        Ok(0)
    }

    /// Place info about an open file into struct stat.
    /// Returns Ok(0) on success, Err(()) on error.
    pub fn sys_fstat(&self, proc: &mut CurrentProc<'_>) -> Result<usize, ()> {
        let (_, f) = argfd(0, proc)?;
        // user pointer to struct stat
        let st = argaddr(1, proc)?;
        // Safe since stat will not access proc's open_files.
        unsafe { (*(f as *const RcFile<'static>)).stat(st.into(), proc) }?;
        Ok(0)
    }

    /// Create the path new as a link to the same inode as old.
    /// Returns Ok(0) on success, Err(()) on error.
    pub fn sys_link(&self, proc: &mut CurrentProc<'_>) -> Result<usize, ()> {
        let mut new: [u8; MAXPATH] = [0; MAXPATH];
        let mut old: [u8; MAXPATH] = [0; MAXPATH];
        let old = argstr(0, &mut old, proc)?;
        let new = argstr(1, &mut new, proc)?;
        self.link(old, new, proc)?;
        Ok(0)
    }

    /// Remove a file.
    /// Returns Ok(0) on success, Err(()) on error.
    pub fn sys_unlink(&self, proc: &mut CurrentProc<'_>) -> Result<usize, ()> {
        let mut path: [u8; MAXPATH] = [0; MAXPATH];
        let path = argstr(0, &mut path, proc)?;
        self.unlink(path, proc)?;
        Ok(0)
    }

    /// Open a file.
    /// Returns Ok(0) on success, Err(()) on error.
    pub fn sys_open(&'static self, proc: &mut CurrentProc<'_>) -> Result<usize, ()> {
        let mut path: [u8; MAXPATH] = [0; MAXPATH];
        let path = argstr(0, &mut path, proc)?;
        let path = Path::new(path);
        let omode = argint(1, proc)?;
        let omode = FcntlFlags::from_bits_truncate(omode);
        self.open(path, omode, proc)
    }

    /// Create a new directory.
    /// Returns Ok(0) on success, Err(()) on error.
    pub fn sys_mkdir(&self, proc: &mut CurrentProc<'_>) -> Result<usize, ()> {
        let mut path: [u8; MAXPATH] = [0; MAXPATH];
        let path = argstr(0, &mut path, proc)?;
        self.mkdir(path, proc)?;
        Ok(0)
    }

    /// Create a new directory.
    /// Returns Ok(0) on success, Err(()) on error.
    pub fn sys_mknod(&self, proc: &mut CurrentProc<'_>) -> Result<usize, ()> {
        let mut path: [u8; MAXPATH] = [0; MAXPATH];
        let path = argstr(0, &mut path, proc)?;
        let major = argint(1, proc)? as u16;
        let minor = argint(2, proc)? as u16;
        self.mknod(path, major, minor, proc)?;
        Ok(0)
    }

    /// Change the current directory.
    /// Returns Ok(0) on success, Err(()) on error.
    pub fn sys_chdir(&'static self, proc: &mut CurrentProc<'_>) -> Result<usize, ()> {
        let mut path: [u8; MAXPATH] = [0; MAXPATH];
        let path = argstr(0, &mut path, proc)?;
        self.chdir(path, proc)?;
        Ok(0)
    }

    /// Load a file and execute it with arguments.
    /// Returns Ok(argc argument to user main) on success, Err(()) on error.
    pub fn sys_exec(&self, proc: &mut CurrentProc<'_>) -> Result<usize, ()> {
        let mut path: [u8; MAXPATH] = [0; MAXPATH];
        let mut args = ArrayVec::<[Page; MAXARG]>::new();
        let path = argstr(0, &mut path, proc)?;
        let uargv = argaddr(1, proc)?;

        let mut success = false;
        for i in 0..MAXARG {
            let uarg = ok_or!(
                fetchaddr((uargv + mem::size_of::<usize>() * i).into(), proc),
                break
            );

            if uarg == 0 {
                success = true;
                break;
            }

            let mut page = some_or!(self.alloc(), break);
            if fetchstr(uarg.into(), &mut page[..], proc).is_err() {
                self.free(page);
                break;
            }
            args.push(page);
        }

        let ret = if success {
            self.exec(Path::new(path), &args, proc)
        } else {
            Err(())
        };

        for page in args.drain(..) {
            self.free(page);
        }

        ret
    }

    /// Create a pipe.
    /// Returns Ok(0) on success, Err(()) on error.
    pub fn sys_pipe(&'static self, proc: &mut CurrentProc<'_>) -> Result<usize, ()> {
        // user pointer to array of two integers
        let fdarray = argaddr(0, proc)?.into();
        self.pipe(fdarray, proc)?;
        Ok(0)
    }
}
