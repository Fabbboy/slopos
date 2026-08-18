//! Declarative macro for defining kernel service interfaces.
//!
//! A service is a `#[repr(C)]` struct of function pointers registered once at
//! init, so `core` can declare an interface without depending on the crate that
//! implements it.
//!
//! # Example
//!
//! ```rust,ignore
//! use slopos_service_core::define_service;
//!
//! define_service! {
//!     my => MyServices {
//!         get_value(id: u32) -> i64;
//!         set_value(id: u32, val: i64) -> bool;
//!         @no_wrapper internal_op(ptr: *const u8, len: usize) -> i32;
//!     }
//! }
//!
//! // Manual wrapper for @no_wrapper method:
//! #[inline(always)]
//! pub fn internal_op_safe(data: &[u8]) -> i32 {
//!     (my_services().internal_op)(data.as_ptr(), data.len())
//! }
//! ```
//!
//! For `my => MyServices` the macro generates `register_my_services`,
//! `is_my_initialized`, `my_services`, and a wrapper per method unless
//! `@no_wrapper`.

#[macro_export]
macro_rules! define_service {
    (
        $(#[$svc_meta:meta])*
        $svc_name:ident => $struct_name:ident {
            $(
                $(#[$method_meta:meta])*
                $(@$attr:ident)?
                $method_name:ident($($arg_name:ident : $arg_ty:ty),* $(,)?) $(-> $ret_ty:ty)?
            );* $(;)?
        }
    ) => {
        $(#[$svc_meta])*
        #[repr(C)]
        pub struct $struct_name {
            $(
                $(#[$method_meta])*
                pub $method_name: fn($($arg_ty),*) $(-> $ret_ty)?,
            )*
        }

        $crate::define_service!(@storage $svc_name, $struct_name);
        $crate::define_service!(@accessors $svc_name, $struct_name);

        $(
            $crate::define_service!(@wrapper
                $svc_name,
                $(@$attr)?
                $method_name($($arg_name : $arg_ty),*) $(-> $ret_ty)?
            );
        )*
    };

    (@storage $svc_name:ident, $struct_name:ident) => {
        $crate::paste::paste! {
            static [<$svc_name:upper>]: $crate::ServiceCell<$struct_name> =
                $crate::ServiceCell::new(stringify!($svc_name));
        }
    };

    (@accessors $svc_name:ident, $struct_name:ident) => {
        $crate::paste::paste! {
            pub fn [<register_ $svc_name _services>](services: &'static $struct_name) {
                [<$svc_name:upper>].register(services);
            }

            #[inline]
            pub fn [<is_ $svc_name _initialized>]() -> bool {
                [<$svc_name:upper>].is_initialized()
            }

            #[inline(always)]
            pub fn [<$svc_name _services>]() -> &'static $struct_name {
                [<$svc_name:upper>].get()
            }
        }
    };

    (@wrapper $svc_name:ident, $method_name:ident($($arg_name:ident : $arg_ty:ty),*) $(-> $ret_ty:ty)?) => {
        $crate::paste::paste! {
            #[inline(always)]
            pub fn $method_name($($arg_name: $arg_ty),*) $(-> $ret_ty)? {
                ([<$svc_name _services>]().$method_name)($($arg_name),*)
            }
        }
    };

    (@wrapper $svc_name:ident, @no_wrapper $method_name:ident($($arg_name:ident : $arg_ty:ty),*) $(-> $ret_ty:ty)?) => {};
}
