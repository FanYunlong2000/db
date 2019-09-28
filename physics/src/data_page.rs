#[repr(C)]
pub struct DataPage {
  pub prev: u32,
  pub next: u32,
  // !0 for none
  pub next_free: u32,
  pub count: u16,
  pub _rsv: [u8; 2],
  pub used: [u32; common::MAX_SLOT_BS],
  pub data: [u8; common::MAX_DATA_BYTE],
}

impl DataPage {
  pub fn init(&mut self, prev: u32, next: u32) {
    self.prev = prev;
    self.next = next;
    self.next_free = !0;
    self.count = 0;
    unsafe { self.used.as_mut_ptr().write_bytes(0, common::MAX_SLOT_BS); }
  }
}

// for simplicity, a check list is one page
#[repr(C)]
pub struct CheckPage {
  pub len: u32,
  pub data: [u8; MAX_CHECK_BYTES],
}

pub const MAX_CHECK_BYTES: usize = 8188;

#[cfg_attr(tarpaulin, skip)]
fn _ck() {
  const_assert_eq!(std::mem::size_of::<DataPage>(), common::PAGE_SIZE);
  const_assert_eq!(std::mem::size_of::<CheckPage>(), common::PAGE_SIZE);
}