use crate::model::*;

#[test]
pub fn test_license_status() {
    // test on movebank responses //
    let status = LicenseStatus::Accepted;
    assert_eq!(status, LicenseStatus::Accepted);    
}

// obtain movebank response test //
pub fn obtainMovebankResponse() {
    // placeholder for response
    
}