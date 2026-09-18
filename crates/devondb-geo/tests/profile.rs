use devondb_geo::{
    FORMAT_V1_PROFILE, Profile,
    grid::{
        atom, atom_in, cell, cell_in, descendant_atom_range, descendant_atom_range_in,
        h3_compat_cell, h3_compat_cell_in,
    },
};

#[test]
fn format_v1_explicitly_pins_profile_zero() {
    assert_eq!(Profile::H3_COMPATIBLE.id(), 0);
    assert_eq!(FORMAT_V1_PROFILE, Profile::H3_COMPATIBLE);
}

#[test]
fn format_v1_shorthands_match_explicit_entry_points() {
    let (lat, lng, resolution) = (37.7749, -122.4194, 9);
    let explicit_atom = atom_in(FORMAT_V1_PROFILE, lat, lng).unwrap();
    let explicit_cell = cell_in(FORMAT_V1_PROFILE, lat, lng, resolution).unwrap();
    let explicit_h3 = h3_compat_cell_in(FORMAT_V1_PROFILE, lat, lng, resolution).unwrap();

    assert_eq!(atom(lat, lng).unwrap(), explicit_atom);
    assert_eq!(cell(lat, lng, resolution).unwrap(), explicit_cell);
    assert_eq!(h3_compat_cell(lat, lng, resolution).unwrap(), explicit_h3);
    assert_eq!(
        descendant_atom_range(explicit_cell),
        descendant_atom_range_in(FORMAT_V1_PROFILE, explicit_cell).unwrap()
    );
}

#[test]
fn every_explicit_entry_point_rejects_an_unimplemented_profile() {
    let profile = Profile::from_id(7);
    let cell = cell(0.0, 0.0, 3).unwrap();
    let errors = [
        atom_in(profile, 0.0, 0.0).unwrap_err(),
        cell_in(profile, 0.0, 0.0, 3).unwrap_err(),
        h3_compat_cell_in(profile, 0.0, 0.0, 3).unwrap_err(),
        descendant_atom_range_in(profile, cell).unwrap_err(),
    ];

    for error in errors {
        assert!(
            error
                .to_string()
                .contains("profile 7 is not implemented by this build"),
            "unexpected error: {error}"
        );
    }
}
