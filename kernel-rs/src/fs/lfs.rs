// TODO: remove it
#![allow(unused_variables)]

use core::pin::Pin;

use super::{FcntlFlags, FileSystem, Inode, InodeGuard, InodeType, Path, RcInode};
use crate::{
    arena::{Arena, ArenaObject},
    proc::KernelCtx,
};

pub struct InodeInner {}

impl ArenaObject for Inode<InodeInner> {
    type Ctx<'a, 'id: 'a> = ();

    #[allow(clippy::needless_lifetimes)]
    fn finalize<'a, 'id: 'a, A: Arena>(&mut self, _: ()) {}
}

pub struct Lfs {}

impl FileSystem for Lfs {
    type Dirent = ();
    type InodeInner = InodeInner;
    type Tx<'s> = &'s ();

    fn init(&self, dev: u32, ctx: &KernelCtx<'_, '_>) {
        todo!()
    }

    fn begin_tx(&self, ctx: &KernelCtx<'_, '_>) -> Self::Tx<'_> {
        todo!()
    }

    fn root(self: Pin<&Self>) -> RcInode<Self::InodeInner> {
        todo!()
    }

    fn namei(
        self: Pin<&Self>,
        path: &Path,
        tx: &Self::Tx<'_>,
        ctx: &KernelCtx<'_, '_>,
    ) -> Result<RcInode<Self::InodeInner>, ()> {
        todo!()
    }

    fn link(
        self: Pin<&Self>,
        inode: RcInode<Self::InodeInner>,
        path: &Path,
        tx: &Self::Tx<'_>,
        ctx: &KernelCtx<'_, '_>,
    ) -> Result<(), ()> {
        todo!()
    }

    fn unlink(
        self: Pin<&Self>,
        path: &Path,
        tx: &Self::Tx<'_>,
        ctx: &KernelCtx<'_, '_>,
    ) -> Result<(), ()> {
        todo!()
    }

    fn create<F, T>(
        self: Pin<&Self>,
        path: &Path,
        typ: InodeType,
        tx: &Self::Tx<'_>,
        ctx: &KernelCtx<'_, '_>,
        f: F,
    ) -> Result<(RcInode<Self::InodeInner>, T), ()>
    where
        F: FnOnce(&mut InodeGuard<'_, Self::InodeInner>) -> T,
    {
        todo!()
    }

    fn open(
        self: Pin<&Self>,
        path: &Path,
        omode: FcntlFlags,
        tx: &Self::Tx<'_>,
        ctx: &mut KernelCtx<'_, '_>,
    ) -> Result<usize, ()> {
        todo!()
    }

    fn chdir(
        self: Pin<&Self>,
        inode: RcInode<Self::InodeInner>,
        tx: &Self::Tx<'_>,
        ctx: &mut KernelCtx<'_, '_>,
    ) -> Result<(), ()> {
        todo!()
    }
}
