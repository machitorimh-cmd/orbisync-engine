//! NonDB harness for the exact pinned private Bind body encoder. The fixture is
//! unmodified SQLx 0.8.6 source; small ID/trait shims expose it only to this test.
//! No production driver fork, sockets, connection, SQL execution or permissions.
#![allow(missing_docs, clippy::unwrap_used)]
use sqlx::{
    Encode, Postgres,
    postgres::{PgArgumentBuffer, PgValueFormat},
};
type Error = sqlx::Error;
macro_rules! err_protocol { ($($arg:tt)*) => { sqlx::Error::Protocol(format!($($arg)*)) }; }
include!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/checkpoint_resource_allocator.rs"
));

mod io {
    use std::num::Saturating;
    #[derive(Debug, Clone, Copy)]
    pub struct PortalId(pub &'static str);
    #[derive(Debug, Clone, Copy)]
    pub struct StatementId(pub &'static str);
    impl PortalId {
        pub fn name_len(self) -> Saturating<usize> {
            Saturating(self.0.len() + 1)
        }
    }
    impl StatementId {
        pub fn name_len(self) -> Saturating<usize> {
            Saturating(self.0.len() + 1)
        }
    }
    pub trait PgBufMutExt {
        fn put_portal_name(&mut self, id: PortalId);
        fn put_statement_name(&mut self, id: StatementId);
    }
    impl PgBufMutExt for Vec<u8> {
        fn put_portal_name(&mut self, id: PortalId) {
            self.extend_from_slice(id.0.as_bytes());
            self.push(0);
        }
        fn put_statement_name(&mut self, id: StatementId) {
            self.extend_from_slice(id.0.as_bytes());
            self.push(0);
        }
    }
}
mod message {
    use std::num::Saturating;
    pub enum FrontendMessageFormat {
        Bind = b'B' as isize,
    }
    pub trait FrontendMessage {
        const FORMAT: FrontendMessageFormat;
        fn body_size_hint(&self) -> Saturating<usize>;
        fn encode_body(&self, buf: &mut Vec<u8>) -> Result<(), crate::Error>;
    }
    pub mod pinned {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/sqlx_0_8_6_bind.rs"
        ));
    }
}
fn argument<'q, T: Encode<'q, Postgres>>(buffer: &mut PgArgumentBuffer, value: T) {
    let offset = buffer.len();
    buffer.extend_from_slice(&[0; 4]);
    let _null = value.encode_by_ref(buffer).unwrap();
    let len = (buffer.len() - offset - 4) as i32;
    buffer[offset..offset + 4].copy_from_slice(&len.to_be_bytes());
}
#[test]
fn r3_pinned_bytea_and_complete_bind_min_default_max() {
    use message::FrontendMessage;
    // Changing the pinned version requires reviewing/replacing the private
    // body fixture, not silently claiming a new driver's resource behavior.
    assert!(
        include_str!("../../../Cargo.lock")
            .contains("name = \"sqlx-postgres\"\nversion = \"0.8.6\"")
    );
    for size in [16384, 262144, 1048576] {
        let input = vec![0x5au8; size];
        let mut capacities = (0, 0);
        let (peak, stack) = tracking::measure(|| {
            let mut args = PgArgumentBuffer::default();
            args.reserve(size + 104);
            argument(&mut args, uuid::Uuid::nil());
            argument(&mut args, uuid::Uuid::nil());
            argument(&mut args, 0i64);
            argument(&mut args, input.as_slice());
            argument(&mut args, size as i64);
            argument(&mut args, &[7u8; 32][..]);
            assert_eq!(args.len(), size + 104);
            let bind = message::pinned::Bind {
                portal: io::PortalId(""),
                statement: io::StatementId("sqlx_s_4294967295"),
                formats: &[PgValueFormat::Binary],
                num_params: 6,
                params: &args,
                result_formats: &[PgValueFormat::Binary],
            };
            let mut wire = Vec::with_capacity(bind.body_size_hint().0 + 5);
            wire.push(<message::pinned::Bind<'_> as FrontendMessage>::FORMAT as u8);
            wire.extend_from_slice(&[0; 4]);
            bind.encode_body(&mut wire).unwrap();
            let len = (wire.len() - 1) as i32;
            wire[1..5].copy_from_slice(&len.to_be_bytes());
            capacities = (args.capacity(), wire.capacity());
            assert_eq!(wire[0], b'B');
            assert_eq!(
                i32::from_be_bytes(wire[1..5].try_into().unwrap()) as usize + 1,
                wire.len()
            );
            // Independently walk complete framing/length prefixes, not just
            // equality with a second serialization of the same body.
            let mut offset = 5;
            for expected in ["", "sqlx_s_4294967295"] {
                let end = wire[offset..].iter().position(|&b| b == 0).unwrap() + offset;
                assert_eq!(&wire[offset..end], expected.as_bytes());
                offset = end + 1;
            }
            assert_eq!(&wire[offset..offset + 6], &[0, 1, 0, 1, 0, 6]);
            offset += 6;
            for expected in [16, 16, 8, size, 8, 32] {
                let length =
                    i32::from_be_bytes(wire[offset..offset + 4].try_into().unwrap()) as usize;
                assert_eq!(length, expected);
                offset += 4;
                if expected == size {
                    assert_eq!(&wire[offset..offset + length], input.as_slice());
                }
                offset += length;
            }
            assert_eq!(&wire[offset..], &[0, 1, 0, 1]);
            assert_eq!(wire.len(), size + 138);
            assert!(wire.capacity() <= 2 * (size + 138));
        });
        println!(
            "R3 BYTEA+Bind C={size} argument_capacity={} wire_capacity={} requested_overlap_excluding_input={peak} native_sample={stack}",
            capacities.0, capacities.1
        );
        assert!(peak <= (4 * size + 1024) as isize);
    }
}
