//! Isochronous stream (CIS) HCI commands not yet defined by `bt-hci`: the peripheral (acceptor)
//! side (`LE Accept/Reject CIS Request`, `LE Setup/Remove ISO Data Path`) and the central
//! (initiator) side (`LE Set CIG Parameters`, `LE Create CIS`, `LE Remove CIG`). Defined with
//! `bt_hci`'s own `cmd!` macro, so they run through `trouble_host`'s `Stack::iso().command(...)`
//! like any other HCI command.

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

cmd! {
    /// LE Set CIG Parameters command [📖](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-54/out/en/host-controller-interface/host-controller-interface-functional-specification.html) (§7.8.97).
    ///
    /// The real command carries a `CIS_Count`-length array of per-CIS records; this crate only
    /// ever builds a 2-CIS stereo CIG, so the array is flattened into `_0`/`_1`-suffixed fields
    /// (`bt_hci::cmd!`'s `Params` are a fixed struct, not a variable-length array) rather than
    /// generalized to N CIS.
    LeSetCigParameters(LE, 0x0062) {
        LeSetCigParametersParams {
            sdu_interval_c_to_p: [u8; 3],
            sdu_interval_p_to_c: [u8; 3],
            worst_case_sca: u8,
            packing: u8,
            framing: u8,
            max_transport_latency_c_to_p: u16,
            max_transport_latency_p_to_c: u16,
            cis_count: u8,
            cis_id_0: u8,
            max_sdu_c_to_p_0: u16,
            max_sdu_p_to_c_0: u16,
            phy_c_to_p_0: u8,
            phy_p_to_c_0: u8,
            rtn_c_to_p_0: u8,
            rtn_p_to_c_0: u8,
            cis_id_1: u8,
            max_sdu_c_to_p_1: u16,
            max_sdu_p_to_c_1: u16,
            phy_c_to_p_1: u8,
            phy_p_to_c_1: u8,
            rtn_c_to_p_1: u8,
            rtn_p_to_c_1: u8,
        }
        LeSetCigParametersReturn {
            num_handles: u8,
            connection_handle_0: ConnHandle,
            connection_handle_1: ConnHandle,
        }
        Handle = cig_id: u8;
    }
}

cmd! {
    /// LE Create CIS command [📖](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-54/out/en/host-controller-interface/host-controller-interface-functional-specification.html) (§7.8.99).
    ///
    /// Async like [`LeAcceptCisRequest`]: completion is per-CIS, via `LE CIS Established` events,
    /// not this command's own (immediate) Command Status. Flattened to exactly 2 CIS - see
    /// [`LeSetCigParameters`].
    LeCreateCis(LE, 0x0064) {
        LeCreateCisParams {
            num_cis: u8,
            cis_connection_handle_0: ConnHandle,
            acl_connection_handle_0: ConnHandle,
            cis_connection_handle_1: ConnHandle,
            acl_connection_handle_1: ConnHandle,
        }
    }
}

cmd! {
    /// LE Remove CIG command [📖](https://www.bluetooth.com/wp-content/uploads/Files/Specification/HTML/Core-54/out/en/host-controller-interface/host-controller-interface-functional-specification.html) (§7.8.100).
    LeRemoveCig(LE, 0x0065) {
        Params = u8;
        Return = u8;
        Handle = u8;
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

    #[test]
    fn set_cig_parameters_encodes_expected_wire_bytes() {
        let cmd = LeSetCigParameters::new(
            0x01,             // cig_id
            [0x10, 0x27, 0x00], // sdu_interval_c_to_p: 10000us
            [0x10, 0x27, 0x00], // sdu_interval_p_to_c: 10000us
            0x00,             // worst_case_sca
            0x00,             // packing: sequential
            0x00,             // framing: unframed
            0x0014,           // max_transport_latency_c_to_p
            0x0014,           // max_transport_latency_p_to_c
            0x02,             // cis_count
            0x00, 0x0064, 0x0000, 0x01, 0x01, 0x02, 0x02, // CIS 0
            0x01, 0x0064, 0x0000, 0x01, 0x01, 0x02, 0x02, // CIS 1
        );
        let mut buf = [0u8; 40];
        let mut w: &mut [u8] = &mut buf;
        cmd.write_hci(&mut w).unwrap();

        #[rustfmt::skip]
        let expected: &[u8] = &[
            0x62, 0x20, // opcode: OGF=LE(8)<<10 | OCF=0x0062, LE bytes
            33,         // params length
            0x01,       // cig_id
            0x10, 0x27, 0x00, // sdu_interval_c_to_p
            0x10, 0x27, 0x00, // sdu_interval_p_to_c
            0x00,       // worst_case_sca
            0x00,       // packing
            0x00,       // framing
            0x14, 0x00, // max_transport_latency_c_to_p
            0x14, 0x00, // max_transport_latency_p_to_c
            0x02,       // cis_count
            0x00, 0x64, 0x00, 0x00, 0x00, 0x01, 0x01, 0x02, 0x02, // CIS 0
            0x01, 0x64, 0x00, 0x00, 0x00, 0x01, 0x01, 0x02, 0x02, // CIS 1
        ];
        assert_eq!(&buf[..expected.len()], expected);
    }

    #[test]
    fn create_cis_encodes_expected_wire_bytes() {
        let cmd = LeCreateCis::new(
            0x02,
            ConnHandle::new(0x0100),
            ConnHandle::new(0x0005),
            ConnHandle::new(0x0101),
            ConnHandle::new(0x0005),
        );
        let mut buf = [0u8; 16];
        let mut w: &mut [u8] = &mut buf;
        cmd.write_hci(&mut w).unwrap();

        #[rustfmt::skip]
        let expected: &[u8] = &[
            0x64, 0x20, // opcode
            9,          // params length
            0x02,       // num_cis
            0x00, 0x01, // cis_connection_handle_0
            0x05, 0x00, // acl_connection_handle_0
            0x01, 0x01, // cis_connection_handle_1
            0x05, 0x00, // acl_connection_handle_1
        ];
        assert_eq!(&buf[..expected.len()], expected);
    }

    #[test]
    fn remove_cig_encodes_expected_wire_bytes() {
        let cmd = LeRemoveCig::new(0x01);
        let mut buf = [0u8; 16];
        let mut w: &mut [u8] = &mut buf;
        cmd.write_hci(&mut w).unwrap();

        let expected: &[u8] = &[0x65, 0x20, 1, 0x01];
        assert_eq!(&buf[..expected.len()], expected);
    }
}
