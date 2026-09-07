//! A latched allocation budget: caught JavaScript OOMs still invalidate a run.

use rquickjs::allocator::{Allocator, RustAllocator};
use std::cell::Cell;
use std::ptr;
use std::rc::Rc;

pub(crate) struct BudgetAllocator {
    inner: RustAllocator,
    used: usize,
    limit: usize,
    exceeded: Rc<Cell<bool>>,
}
impl BudgetAllocator {
    pub(crate) fn new(limit: usize, exceeded: Rc<Cell<bool>>) -> Self {
        Self {
            inner: RustAllocator,
            used: 0,
            limit,
            exceeded,
        }
    }
    fn permits(&self, old: usize, requested: usize) -> bool {
        // Include conservative alignment/header overhead rather than counting only
        // payload bytes. RustAllocator stores an aligned usize before each block.
        let charge = requested.checked_add(32);
        let allowed = charge.is_some_and(|n| n <= self.limit.saturating_sub(self.used - old));
        if !allowed {
            self.exceeded.set(true);
        }
        allowed
    }
    unsafe fn charge(ptr: *mut u8) -> usize {
        // SAFETY: caller only passes a non-null block from RustAllocator.
        unsafe { RustAllocator::usable_size(ptr) + 16 }
    }
}
// SAFETY: allocation, alignment and usable-size contracts are delegated unchanged
// to RustAllocator. A denied allocation returns null without changing the old
// allocation. Accounting never dereferences memory after free/realloc.
unsafe impl Allocator for BudgetAllocator {
    fn alloc(&mut self, size: usize) -> *mut u8 {
        if !self.permits(0, size) {
            return ptr::null_mut();
        }
        let ptr = self.inner.alloc(size);
        if ptr.is_null() {
            self.exceeded.set(true);
        } else {
            self.used += unsafe { Self::charge(ptr) };
        }
        ptr
    }
    fn calloc(&mut self, count: usize, size: usize) -> *mut u8 {
        let Some(total) = count.checked_mul(size) else {
            self.exceeded.set(true);
            return ptr::null_mut();
        };
        if !self.permits(0, total) {
            return ptr::null_mut();
        }
        let ptr = self.inner.calloc(count, size);
        if ptr.is_null() {
            if total != 0 {
                self.exceeded.set(true);
            }
        } else {
            self.used += unsafe { Self::charge(ptr) };
        }
        ptr
    }
    unsafe fn dealloc(&mut self, ptr: *mut u8) {
        if ptr.is_null() {
            return;
        }
        self.used -= unsafe { Self::charge(ptr) };
        unsafe {
            self.inner.dealloc(ptr);
        }
    }
    unsafe fn realloc(&mut self, ptr: *mut u8, size: usize) -> *mut u8 {
        if ptr.is_null() {
            return self.alloc(size);
        }
        if size == 0 {
            unsafe {
                self.dealloc(ptr);
            }
            return ptr::null_mut();
        }
        let old = unsafe { Self::charge(ptr) };
        if !self.permits(old, size) {
            return ptr::null_mut();
        }
        let next = unsafe { self.inner.realloc(ptr, size) };
        if next.is_null() {
            self.exceeded.set(true);
        } else {
            self.used = self.used - old + unsafe { Self::charge(next) };
        }
        next
    }
    unsafe fn usable_size(ptr: *mut u8) -> usize {
        unsafe { RustAllocator::usable_size(ptr) }
    }
}
