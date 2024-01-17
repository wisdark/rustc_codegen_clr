#![feature(lang_items,adt_const_params,associated_type_defaults,core_intrinsics,start)]
#![allow(internal_features,incomplete_features,unused_variables,dead_code)]
#![no_std]
include!("../common.rs");
fn funny_swap(num:u32)->u32{
    ((num << 24) | (num >> 24)) | (((num << 8) & 0xFF0000) | ((num >> 8) & 0xFF00))
}
fn main(){
    test_eq!(black_box(0x01_u8),core::intrinsics::bswap(black_box(0x01_u8)));
    test_eq!(black_box(0x2301_u16),core::intrinsics::bswap(black_box(0x0123_u16)));
    test_eq!(black_box(0xFF_00_00_00_u32),core::intrinsics::bswap(black_box(0x00_00_00_FF_u32)));
    test_eq!(black_box(0x00_FF_00_00_u32),core::intrinsics::bswap(black_box(0x00_00_FF_00_u32)));
    test_eq!(black_box(0x00_00_FF_00_u32),core::intrinsics::bswap(black_box(0x00_FF_00_00_u32)));
    test_eq!(black_box(0x00_00_00_FF_u32),core::intrinsics::bswap(black_box(0xFF_00_00_00_u32)));
    //test_eq!(black_box(0x000000FF_u32),core::intrinsics::bswap(black_box(0xFF000000_u32)));
    //test_eq!(black_box(0x67452301_u32),core::intrinsics::bswap(black_box(0x01234567_u32)));
}
    