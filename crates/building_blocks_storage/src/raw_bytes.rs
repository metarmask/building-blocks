use std::ops::Deref;

pub trait AsRawBytes<'a> {
    type Output: Deref<Target = [u8]>;

    fn as_raw_bytes(&'a self) -> Self::Output;
}

impl<'a, T> AsRawBytes<'a> for [T]
where
    T: 'static + Copy,
{
    type Output = &'a [u8];

    fn as_raw_bytes(&'a self) -> Self::Output {
        unsafe {
            std::slice::from_raw_parts(
                self.as_ptr() as *const u8,
                self.len() * core::mem::size_of::<T>(),
            )
        }
    }
}
