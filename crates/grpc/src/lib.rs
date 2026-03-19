pub mod service;
pub mod evaluate;
pub mod server;
pub mod code_snip;

/// Generated protobuf code.
pub mod proto {
    include!("generated/provider.rs");

    pub(crate) const FILE_DESCRIPTOR_SET: &[u8] =
        include_bytes!("generated/provider_service_descriptor.bin");
}
