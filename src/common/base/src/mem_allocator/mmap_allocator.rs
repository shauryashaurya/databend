// Copyright 2022 Datafuse Labs.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[derive(Debug, Clone, Copy, Default)]
pub struct MmapAllocator<T> {
    allocator: T,
}

impl<T> MmapAllocator<T> {
    pub fn new(allocator: T) -> Self {
        Self { allocator }
    }
}

#[cfg(target_os = "linux")]
pub mod linux {
    use std::alloc::AllocError;
    use std::alloc::Allocator;
    use std::alloc::Layout;
    use std::ptr::null_mut;
    use std::ptr::NonNull;

    use super::MmapAllocator;

    // MADV_POPULATE_WRITE is supported since Linux 5.14.
    const MADV_POPULATE_WRITE: i32 = 23;

    const THRESHOLD: usize = 64 << 20;

    impl<T> MmapAllocator<T> {
        pub const FALLBACK: bool = false;
    }

    impl<T: Allocator> MmapAllocator<T> {
        #[inline(always)]
        fn mmap_alloc(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            debug_assert!(layout.align() <= page_size());
            const PROT: i32 = libc::PROT_READ | libc::PROT_WRITE;
            const FLAGS: i32 = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_POPULATE;
            let addr = unsafe { libc::mmap(null_mut(), layout.size(), PROT, FLAGS, -1, 0) };
            if addr == libc::MAP_FAILED {
                return Err(AllocError);
            }
            let addr = NonNull::new(addr as *mut ()).ok_or(AllocError)?;
            Ok(NonNull::<[u8]>::from_raw_parts(addr, layout.size()))
        }

        #[inline(always)]
        unsafe fn mmap_dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
            debug_assert!(layout.align() <= page_size());
            let result = libc::munmap(ptr.cast().as_ptr(), layout.size());
            assert_eq!(result, 0, "Failed to deallocate.");
        }

        #[inline(always)]
        unsafe fn mmap_grow(
            &self,
            ptr: NonNull<u8>,
            old_layout: Layout,
            new_layout: Layout,
        ) -> Result<NonNull<[u8]>, AllocError> {
            debug_assert!(old_layout.align() <= page_size());
            debug_assert!(old_layout.align() == new_layout.align());
            const REMAP_FLAGS: i32 = libc::MREMAP_MAYMOVE;
            let addr = libc::mremap(
                ptr.cast().as_ptr(),
                old_layout.size(),
                new_layout.size(),
                REMAP_FLAGS,
            );
            if addr == libc::MAP_FAILED {
                return Err(AllocError);
            }
            let addr = NonNull::new(addr as *mut ()).ok_or(AllocError)?;
            if linux_kernel_version() >= (5, 14, 0) {
                libc::madvise(addr.cast().as_ptr(), new_layout.size(), MADV_POPULATE_WRITE);
            }
            Ok(NonNull::<[u8]>::from_raw_parts(addr, new_layout.size()))
        }

        #[inline(always)]
        unsafe fn mmap_shrink(
            &self,
            ptr: NonNull<u8>,
            old_layout: Layout,
            new_layout: Layout,
        ) -> Result<NonNull<[u8]>, AllocError> {
            debug_assert!(old_layout.align() <= page_size());
            debug_assert!(old_layout.align() == new_layout.align());
            const REMAP_FLAGS: i32 = libc::MREMAP_MAYMOVE;
            let addr = libc::mremap(
                ptr.cast().as_ptr(),
                old_layout.size(),
                new_layout.size(),
                REMAP_FLAGS,
            );
            if addr == libc::MAP_FAILED {
                return Err(AllocError);
            }
            let addr = NonNull::new(addr as *mut ()).ok_or(AllocError)?;
            Ok(NonNull::<[u8]>::from_raw_parts(addr, new_layout.size()))
        }
    }

    unsafe impl<T: Allocator> Allocator for MmapAllocator<T> {
        #[inline(always)]
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            if layout.align() > page_size() {
                return self.allocator.allocate(layout);
            }
            if layout.size() >= THRESHOLD {
                self.mmap_alloc(layout)
            } else {
                self.allocator.allocate(layout)
            }
        }

        #[inline(always)]
        unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
            if layout.align() > page_size() {
                return self.allocator.deallocate(ptr, layout);
            }
            if layout.size() >= THRESHOLD {
                self.mmap_dealloc(ptr, layout);
            } else {
                self.allocator.deallocate(ptr, layout);
            }
        }

        #[inline(always)]
        fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            if layout.align() > page_size() {
                return self.allocator.allocate_zeroed(layout);
            }
            if layout.size() >= THRESHOLD {
                self.mmap_alloc(layout)
            } else {
                self.allocator.allocate_zeroed(layout)
            }
        }

        unsafe fn grow(
            &self,
            ptr: NonNull<u8>,
            old_layout: Layout,
            new_layout: Layout,
        ) -> Result<NonNull<[u8]>, AllocError> {
            if old_layout.align() > page_size() {
                return self.allocator.grow(ptr, old_layout, new_layout);
            }
            if old_layout.size() >= THRESHOLD {
                self.mmap_grow(ptr, old_layout, new_layout)
            } else if new_layout.size() >= THRESHOLD {
                let addr = self.mmap_alloc(new_layout)?;
                std::ptr::copy_nonoverlapping(
                    ptr.as_ptr(),
                    addr.cast().as_ptr(),
                    old_layout.size(),
                );
                self.allocator.deallocate(ptr, old_layout);
                Ok(addr)
            } else {
                self.allocator.grow(ptr, old_layout, new_layout)
            }
        }

        unsafe fn grow_zeroed(
            &self,
            ptr: NonNull<u8>,
            old_layout: Layout,
            new_layout: Layout,
        ) -> Result<NonNull<[u8]>, AllocError> {
            if old_layout.align() > page_size() {
                return self.allocator.grow_zeroed(ptr, old_layout, new_layout);
            }
            if old_layout.size() >= THRESHOLD {
                self.mmap_grow(ptr, old_layout, new_layout)
            } else if new_layout.size() >= THRESHOLD {
                let addr = self.mmap_alloc(new_layout)?;
                std::ptr::copy_nonoverlapping(
                    ptr.as_ptr(),
                    addr.cast().as_ptr(),
                    old_layout.size(),
                );
                self.allocator.deallocate(ptr, old_layout);
                Ok(addr)
            } else {
                self.allocator.grow_zeroed(ptr, old_layout, new_layout)
            }
        }

        unsafe fn shrink(
            &self,
            ptr: NonNull<u8>,
            old_layout: Layout,
            new_layout: Layout,
        ) -> Result<NonNull<[u8]>, AllocError> {
            if old_layout.align() > page_size() {
                return self.allocator.shrink(ptr, old_layout, new_layout);
            }
            if new_layout.size() >= THRESHOLD {
                self.mmap_shrink(ptr, old_layout, new_layout)
            } else if old_layout.size() >= THRESHOLD {
                let addr = self.allocator.allocate(new_layout)?;
                std::ptr::copy_nonoverlapping(
                    ptr.as_ptr(),
                    addr.cast().as_ptr(),
                    old_layout.size(),
                );
                self.mmap_dealloc(ptr, old_layout);
                Ok(addr)
            } else {
                self.allocator.shrink(ptr, old_layout, new_layout)
            }
        }
    }

    #[inline(always)]
    fn page_size() -> usize {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        const INVAILED: usize = 0;
        static CACHE: AtomicUsize = AtomicUsize::new(INVAILED);
        let fetch = CACHE.load(Ordering::Relaxed);
        if fetch == INVAILED {
            let result = unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) as usize };
            debug_assert_eq!(result.count_ones(), 1);
            CACHE.store(result, Ordering::Relaxed);
            result
        } else {
            fetch
        }
    }

    #[inline(always)]
    fn linux_kernel_version() -> (u16, u8, u8) {
        use std::sync::atomic::AtomicU32;
        use std::sync::atomic::Ordering;
        const INVAILED: u32 = 0;
        static CACHE: AtomicU32 = AtomicU32::new(INVAILED);
        let fetch = CACHE.load(Ordering::Relaxed);
        let code = if fetch == INVAILED {
            let mut uname = unsafe { std::mem::zeroed::<libc::utsname>() };
            assert_ne!(-1, unsafe { libc::uname(&mut uname) });
            let mut length = 0usize;
            while length < uname.release.len() && uname.release[length] != 0 {
                length += 1;
            }
            let slice = unsafe { &*(&uname.release[..length] as *const _ as *const [u8]) };
            let ver = std::str::from_utf8(slice).unwrap();
            let semver = semver::Version::parse(ver).unwrap();
            let result = (semver.major.min(65535) as u32) << 16
                | (semver.minor.min(255) as u32) << 8
                | (semver.patch.min(255) as u32);
            CACHE.store(result, Ordering::Relaxed);
            result
        } else {
            fetch
        };
        ((code >> 16) as u16, (code >> 8) as u8, code as u8)
    }
}

#[cfg(not(target_os = "linux"))]
pub mod fallback {
    use std::alloc::AllocError;
    use std::alloc::Allocator;
    use std::alloc::Layout;
    use std::ptr::NonNull;

    use super::MmapAllocator;

    impl<T> MmapAllocator<T> {
        pub const FALLBACK: bool = true;
    }

    unsafe impl<T: Allocator> Allocator for MmapAllocator<T> {
        #[inline(always)]
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            self.allocator.allocate(layout)
        }

        #[inline(always)]
        unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
            self.allocator.deallocate(ptr, layout)
        }

        #[inline(always)]
        fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            self.allocator.allocate_zeroed(layout)
        }

        unsafe fn grow(
            &self,
            ptr: NonNull<u8>,
            old_layout: Layout,
            new_layout: Layout,
        ) -> Result<NonNull<[u8]>, AllocError> {
            self.allocator.grow(ptr, old_layout, new_layout)
        }

        unsafe fn grow_zeroed(
            &self,
            ptr: NonNull<u8>,
            old_layout: Layout,
            new_layout: Layout,
        ) -> Result<NonNull<[u8]>, AllocError> {
            self.allocator.grow_zeroed(ptr, old_layout, new_layout)
        }

        unsafe fn shrink(
            &self,
            ptr: NonNull<u8>,
            old_layout: Layout,
            new_layout: Layout,
        ) -> Result<NonNull<[u8]>, AllocError> {
            self.allocator.shrink(ptr, old_layout, new_layout)
        }
    }
}
