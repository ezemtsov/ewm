//! Both data-control globals are advertised: wlr for older clients, ext for dms.

use ewm_core::testing::Fixture;

#[test]
fn advertises_wlr_and_ext_data_control() {
    let mut fix = Fixture::new().unwrap();
    let mut client = fix.new_client().unwrap();
    fix.roundtrip(&mut client);
    assert!(client.has_global("zwlr_data_control_manager_v1"));
    assert!(client.has_global("ext_data_control_manager_v1"));
}
