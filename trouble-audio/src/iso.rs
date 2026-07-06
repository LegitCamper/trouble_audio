//! Isochronous stream (CIS) HCI commands not yet defined by `bt-hci`: `LE Accept/Reject CIS
//! Request` and `LE Setup/Remove ISO Data Path` - the peripheral (acceptor) side of CIS setup
//! this crate's sink/source examples need. Defined with `bt_hci`'s own `cmd!` macro, so they run
//! through `trouble_host`'s `Stack::iso().command(...)` like any other HCI command.
//!
//! `LE Set CIG Parameters`/`LE Create CIS`/`LE Remove CIG` (the central/initiator side) aren't
//! defined yet.

use bt_hci::cmd;
use bt_hci::param::ConnHandle;

cmd! {
    /// LE Accept CIS Request command [📖](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-54/out/en/host-controller-interface/host-controller-interface-functional-specification.html#UUID-e7bb8a8f-45bd-2308-1e4d-0d0a92cb0473)
    LeAcceptCisRequest(LE, 0x0066) {
        Params = ConnHandle;
    }
}

cmd! {
    /// LE Reject CIS Request command [📖](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-54/out/en/host-controller-interface/host-controller-interface-functional-specification.html#UUID-9426926e-b73a-2f7d-a9cc-24c3a2d6d3e1)
    LeRejectCisRequest(LE, 0x0067) {
        LeRejectCisRequestParams {
            reason: u8,
        }
        Return = ConnHandle;
        Handle = cis_handle: ConnHandle;
    }
}

cmd! {
    /// LE Setup ISO Data Path command [📖](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-54/out/en/host-controller-interface/host-controller-interface-functional-specification.html#UUID-9b3823f9-b840-3e7d-7a3a-2e9a3c6cf1e1)
    LeSetupIsoDataPath(LE, 0x006e) {
        LeSetupIsoDataPathParams<'a> {
            data_path_direction: u8,
            data_path_id: u8,
            coding_format: u8,
            company_id: u16,
            vendor_specific_codec_id: u16,
            controller_delay: [u8; 3],
            codec_configuration: &'a [u8],
        }
        Return = ConnHandle;
        Handle = connection_handle: ConnHandle;
    }
}

cmd! {
    /// LE Remove ISO Data Path command [📖](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-54/out/en/host-controller-interface/host-controller-interface-functional-specification.html#UUID-9b3823f9-b840-3e7d-7a3a-2e9a3c6cf1e2)
    LeRemoveIsoDataPath(LE, 0x006f) {
        LeRemoveIsoDataPathParams {
            data_path_direction: u8,
        }
        Return = ConnHandle;
        Handle = connection_handle: ConnHandle;
    }
}

/// `Data_Path_Direction` for [`LeSetupIsoDataPath`]/[`LeRemoveIsoDataPath`].
pub mod data_path_direction {
    /// Host to Controller (e.g. a source device's encoded frames going out over the air).
    pub const INPUT: u8 = 0x00;
    /// Controller to Host (e.g. a sink device's received frames).
    pub const OUTPUT: u8 = 0x01;
}

/// `Data_Path_ID` for [`LeSetupIsoDataPath`]: the standard HCI transport, as opposed to a
/// vendor-specific one.
pub const DATA_PATH_ID_HCI: u8 = 0x00;

#[cfg(test)]
mod tests {
    use super::*;
    use bt_hci::WriteHci;

    #[test]
    fn setup_iso_data_path_encodes_expected_wire_bytes() {
        let cmd = LeSetupIsoDataPath::new(
            ConnHandle::new(0x0011),
            data_path_direction::OUTPUT,
            DATA_PATH_ID_HCI,
            0x06, // LC3
            0,
            0,
            [0, 0, 0],
            &[0xAA, 0xBB],
        );
        let mut buf = [0u8; 32];
        let mut w: &mut [u8] = &mut buf;
        cmd.write_hci(&mut w).unwrap();

        #[rustfmt::skip]
        let expected: &[u8] = &[
            0x6e, 0x20, // opcode: OGF=LE(8)<<10 | OCF=0x006e, LE bytes
            15,         // params length
            0x11, 0x00, // connection_handle
            0x01,       // data_path_direction: Output
            0x00,       // data_path_id: HCI
            0x06, 0x00, 0x00, 0x00, 0x00, // codec_id: LC3, no company/vendor id
            0, 0, 0,    // controller_delay
            2,          // codec_configuration length
            0xAA, 0xBB, // codec_configuration
        ];
        assert_eq!(&buf[..expected.len()], expected);
    }

    #[test]
    fn reject_cis_request_encodes_expected_wire_bytes() {
        let cmd = LeRejectCisRequest::new(ConnHandle::new(0x0022), 0x3a);
        let mut buf = [0u8; 16];
        let mut w: &mut [u8] = &mut buf;
        cmd.write_hci(&mut w).unwrap();

        #[rustfmt::skip]
        let expected: &[u8] = &[
            0x67, 0x20, // opcode
            3,          // params length
            0x22, 0x00, // cis_handle
            0x3a,       // reason
        ];
        assert_eq!(&buf[..expected.len()], expected);
    }

    #[test]
    fn accept_cis_request_encodes_expected_wire_bytes() {
        let cmd = LeAcceptCisRequest::new(ConnHandle::new(0x0033));
        let mut buf = [0u8; 16];
        let mut w: &mut [u8] = &mut buf;
        cmd.write_hci(&mut w).unwrap();

        let expected: &[u8] = &[0x66, 0x20, 2, 0x33, 0x00];
        assert_eq!(&buf[..expected.len()], expected);
    }
}
