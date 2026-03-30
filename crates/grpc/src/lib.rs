pub mod code_snip;
pub mod evaluate;
pub mod server;
pub mod service;

/// Generated protobuf code.
pub mod proto {
    include!("generated/provider.rs");

    pub(crate) const FILE_DESCRIPTOR_SET: &[u8] =
        include_bytes!("generated/provider_service_descriptor.bin");
}
