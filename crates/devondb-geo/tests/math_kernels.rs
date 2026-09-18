use core::f64::consts::{FRAC_PI_2, PI};

use devondb_geo::math::{Vec3, acos_det, asin_det, atan2_det, cos_det, sin_det, sqrt_det};

const SPECIAL_INPUTS: [u64; 24] = [
    0x0000_0000_0000_0000,
    0x8000_0000_0000_0000,
    0x3ff0_0000_0000_0000,
    0xbff0_0000_0000_0000,
    0x4009_21fb_5444_2d17,
    0x4009_21fb_5444_2d18,
    0x4009_21fb_5444_2d19,
    0xc009_21fb_5444_2d17,
    0xc009_21fb_5444_2d18,
    0xc009_21fb_5444_2d19,
    0x0000_0000_0000_0001,
    0x8000_0000_0000_0001,
    0x000f_ffff_ffff_ffff,
    0x800f_ffff_ffff_ffff,
    0x0010_0000_0000_0000,
    0x8010_0000_0000_0000,
    0x7fef_ffff_ffff_ffff,
    0xffef_ffff_ffff_ffff,
    0x7ff0_0000_0000_0000,
    0xfff0_0000_0000_0000,
    0x7ff8_0000_0000_0000,
    0xfff8_0000_0000_0000,
    0x7ff8_0000_0000_0042,
    0xfff8_0000_0000_0042,
];

const ATAN2_SPECIAL_INPUTS: [(u64, u64); 24] = [
    (0x0000_0000_0000_0000, 0x3ff0_0000_0000_0000),
    (0x8000_0000_0000_0000, 0x3ff0_0000_0000_0000),
    (0x0000_0000_0000_0000, 0xbff0_0000_0000_0000),
    (0x8000_0000_0000_0000, 0xbff0_0000_0000_0000),
    (0x3ff0_0000_0000_0000, 0x0000_0000_0000_0000),
    (0xbff0_0000_0000_0000, 0x0000_0000_0000_0000),
    (0x3ff0_0000_0000_0000, 0x3ff0_0000_0000_0000),
    (0xbff0_0000_0000_0000, 0x3ff0_0000_0000_0000),
    (0x3ff0_0000_0000_0000, 0xbff0_0000_0000_0000),
    (0xbff0_0000_0000_0000, 0xbff0_0000_0000_0000),
    (0x4009_21fb_5444_2d17, 0x3ff0_0000_0000_0000),
    (0xc009_21fb_5444_2d19, 0x3ff0_0000_0000_0000),
    (0x0000_0000_0000_0001, 0x3ff0_0000_0000_0000),
    (0x800f_ffff_ffff_ffff, 0xbff0_0000_0000_0000),
    (0x7ff0_0000_0000_0000, 0x3ff0_0000_0000_0000),
    (0xfff0_0000_0000_0000, 0x3ff0_0000_0000_0000),
    (0x3ff0_0000_0000_0000, 0x7ff0_0000_0000_0000),
    (0xbff0_0000_0000_0000, 0x7ff0_0000_0000_0000),
    (0x3ff0_0000_0000_0000, 0xfff0_0000_0000_0000),
    (0xbff0_0000_0000_0000, 0xfff0_0000_0000_0000),
    (0x7ff0_0000_0000_0000, 0x7ff0_0000_0000_0000),
    (0xfff0_0000_0000_0000, 0xfff0_0000_0000_0000),
    (0x7ff8_0000_0000_0042, 0x3ff0_0000_0000_0000),
    (0x3ff0_0000_0000_0000, 0xfff8_0000_0000_0000),
];

const SEEDED_CASES: usize = 16;
const SWEEP_SEED: u64 = 0x6a09_e667_f3bc_c909;

// Frozen reference values. The first 24 rows are the special-value sweep
// above; the final 16 come from SplitMix64(SWEEP_SEED).
const SIN_GOLDENS: &[(u64, u64)] = &[
    (0x0000000000000000, 0x0000000000000000),
    (0x8000000000000000, 0x8000000000000000),
    (0x3ff0000000000000, 0x3feaed548f090cee),
    (0xbff0000000000000, 0xbfeaed548f090cee),
    (0x400921fb54442d17, 0x3cc469898cc51702),
    (0x400921fb54442d18, 0x3ca1a62633145c07),
    (0x400921fb54442d19, 0xbcb72cece675d1fd),
    (0xc00921fb54442d17, 0xbcc469898cc51702),
    (0xc00921fb54442d18, 0xbca1a62633145c07),
    (0xc00921fb54442d19, 0x3cb72cece675d1fd),
    (0x0000000000000001, 0x0000000000000001),
    (0x8000000000000001, 0x8000000000000001),
    (0x000fffffffffffff, 0x000fffffffffffff),
    (0x800fffffffffffff, 0x800fffffffffffff),
    (0x0010000000000000, 0x0010000000000000),
    (0x8010000000000000, 0x8010000000000000),
    (0x7fefffffffffffff, 0x3f7452fc98b34e97),
    (0xffefffffffffffff, 0xbf7452fc98b34e97),
    (0x7ff0000000000000, 0x7ff8000000000000),
    (0xfff0000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0xfff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000042, 0x7ff8000000000000),
    (0xfff8000000000042, 0x7ff8000000000000),
    (0x3fefc62a2b097592, 0x3feacde905747e2f),
    (0xbfe746b419466aec, 0xbfe546fd0e87efa2),
    (0x3fe64674f98aa19e, 0x3fe484f5b7695f37),
    (0x3fe4eb47b26de7ac, 0x3fe375ead77ba5ee),
    (0xbfe384ad339cfcc3, 0xbfe2549117c20cb5),
    (0x3fe720d059892bc4, 0x3fe52aa13ed3be25),
    (0xbfe675c92d60f3df, 0xbfe4a93091d31150),
    (0x3fe9c7b9c3a56969, 0x3fe7149fb27fea2c),
    (0xbfe685b6014a22c9, 0xbfe4b5573369b624),
    (0x3fe5c47ec47e016d, 0x3fe42093b9fc17c1),
    (0xbfe314ef3d87ae57, 0xbfe1f88a5bc34c72),
    (0xbfe2311ba18cfd93, 0xbfe13a4742938dee),
    (0x3fed7011d7bc72c9, 0x3fe97539139711a5),
    (0x3fe89b9538c27512, 0x3fe640cf22486608),
    (0xbfe38806108a16a3, 0xbfe2574f585ace5e),
    (0x3fe69fb3420a4216, 0x3fe4c920a1bab8fb),
];

const COS_GOLDENS: &[(u64, u64)] = &[
    (0x0000000000000000, 0x3ff0000000000000),
    (0x8000000000000000, 0x3ff0000000000000),
    (0x3ff0000000000000, 0x3fe14a280fb5068c),
    (0xbff0000000000000, 0x3fe14a280fb5068c),
    (0x400921fb54442d17, 0xbff0000000000000),
    (0x400921fb54442d18, 0xbff0000000000000),
    (0x400921fb54442d19, 0xbff0000000000000),
    (0xc00921fb54442d17, 0xbff0000000000000),
    (0xc00921fb54442d18, 0xbff0000000000000),
    (0xc00921fb54442d19, 0xbff0000000000000),
    (0x0000000000000001, 0x3ff0000000000000),
    (0x8000000000000001, 0x3ff0000000000000),
    (0x000fffffffffffff, 0x3ff0000000000000),
    (0x800fffffffffffff, 0x3ff0000000000000),
    (0x0010000000000000, 0x3ff0000000000000),
    (0x8010000000000000, 0x3ff0000000000000),
    (0x7fefffffffffffff, 0xbfefffe62ecfab75),
    (0xffefffffffffffff, 0xbfefffe62ecfab75),
    (0x7ff0000000000000, 0x7ff8000000000000),
    (0xfff0000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0xfff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000042, 0x7ff8000000000000),
    (0xfff8000000000042, 0x7ff8000000000000),
    (0x3fefc62a2b097592, 0x3fe17ab665b91b6d),
    (0xbfe746b419466aec, 0x3fe7e6c1a563d5e6),
    (0x3fe64674f98aa19e, 0x3fe88e1e284c34a8),
    (0x3fe4eb47b26de7ac, 0x3fe96706fa5a213c),
    (0xbfe384ad339cfcc3, 0x3fea3acf8225f637),
    (0x3fe720d059892bc4, 0x3fe7ffe261fe2b00),
    (0xbfe675c92d60f3df, 0x3fe86faa10b4504b),
    (0x3fe9c7b9c3a56969, 0x3fe62a3e1825ec0c),
    (0xbfe685b6014a22c9, 0x3fe8655ed31ca94a),
    (0x3fe5c47ec47e016d, 0x3fe8e0a8a0c277a9),
    (0xbfe314ef3d87ae57, 0x3fea7a313d737504),
    (0xbfe2311ba18cfd93, 0x3feaf77fef946dba),
    (0x3fed7011d7bc72c9, 0x3fe363551b11bcd9),
    (0x3fe89b9538c27512, 0x3fe6fede88f22b55),
    (0xbfe38806108a16a3, 0x3fea38e488b4f94d),
    (0x3fe69fb3420a4216, 0x3fe8548539738d34),
];

const ASIN_GOLDENS: &[(u64, u64)] = &[
    (0x0000000000000000, 0x0000000000000000),
    (0x8000000000000000, 0x8000000000000000),
    (0x3ff0000000000000, 0x3ff921fb54442d18),
    (0xbff0000000000000, 0xbff921fb54442d18),
    (0x400921fb54442d17, 0x7ff8000000000000),
    (0x400921fb54442d18, 0x7ff8000000000000),
    (0x400921fb54442d19, 0x7ff8000000000000),
    (0xc00921fb54442d17, 0x7ff8000000000000),
    (0xc00921fb54442d18, 0x7ff8000000000000),
    (0xc00921fb54442d19, 0x7ff8000000000000),
    (0x0000000000000001, 0x0000000000000001),
    (0x8000000000000001, 0x8000000000000001),
    (0x000fffffffffffff, 0x000fffffffffffff),
    (0x800fffffffffffff, 0x800fffffffffffff),
    (0x0010000000000000, 0x0010000000000000),
    (0x8010000000000000, 0x8010000000000000),
    (0x7fefffffffffffff, 0x7ff8000000000000),
    (0xffefffffffffffff, 0x7ff8000000000000),
    (0x7ff0000000000000, 0x7ff8000000000000),
    (0xfff0000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0xfff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000042, 0x7ff8000000000000),
    (0xfff8000000000042, 0x7ff8000000000000),
    (0x3fefc62a2b097592, 0x3ff73afa636bc351),
    (0xbfe746b419466aec, 0xbfea105ce217e7ee),
    (0x3fe64674f98aa19e, 0x3fe8a37225de70f2),
    (0x3fe4eb47b26de7ac, 0x3fe6ccb99b6b9cf0),
    (0xbfe384ad339cfcc3, 0xbfe4fde386847d7c),
    (0x3fe720d059892bc4, 0x3fe9d958283c4839),
    (0xbfe675c92d60f3df, 0xbfe8e5a0ef97263d),
    (0x3fe9c7b9c3a56969, 0x3fedf9c447d5ce4a),
    (0xbfe685b6014a22c9, 0xbfe8fc04275f9628),
    (0x3fe5c47ec47e016d, 0x3fe7f04fac58898b),
    (0xbfe314ef3d87ae57, 0xbfe471cb5e40fa9e),
    (0xbfe2311ba18cfd93, 0xbfe3597f197ba644),
    (0x3fed7011d7bc72c9, 0x3ff2afbaf11e289a),
    (0x3fe89b9538c27512, 0x3fec1289a9a20817),
    (0xbfe38806108a16a3, 0xbfe5021d086f409a),
    (0x3fe69fb3420a4216, 0x3fe920aed05e5bc3),
];

const ACOS_GOLDENS: &[(u64, u64)] = &[
    (0x0000000000000000, 0x3ff921fb54442d18),
    (0x8000000000000000, 0x3ff921fb54442d18),
    (0x3ff0000000000000, 0x0000000000000000),
    (0xbff0000000000000, 0x400921fb54442d18),
    (0x400921fb54442d17, 0x7ff8000000000000),
    (0x400921fb54442d18, 0x7ff8000000000000),
    (0x400921fb54442d19, 0x7ff8000000000000),
    (0xc00921fb54442d17, 0x7ff8000000000000),
    (0xc00921fb54442d18, 0x7ff8000000000000),
    (0xc00921fb54442d19, 0x7ff8000000000000),
    (0x0000000000000001, 0x3ff921fb54442d18),
    (0x8000000000000001, 0x3ff921fb54442d18),
    (0x000fffffffffffff, 0x3ff921fb54442d18),
    (0x800fffffffffffff, 0x3ff921fb54442d18),
    (0x0010000000000000, 0x3ff921fb54442d18),
    (0x8010000000000000, 0x3ff921fb54442d18),
    (0x7fefffffffffffff, 0x7ff8000000000000),
    (0xffefffffffffffff, 0x7ff8000000000000),
    (0x7ff0000000000000, 0x7ff8000000000000),
    (0xfff0000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0xfff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000042, 0x7ff8000000000000),
    (0xfff8000000000042, 0x7ff8000000000000),
    (0x3fefc62a2b097592, 0x3fbe700f0d869c76),
    (0xbfe746b419466aec, 0x40031514e2a81088),
    (0x3fe64674f98aa19e, 0x3fe9a08482a9e93f),
    (0x3fe4eb47b26de7ac, 0x3feb773d0d1cbd40),
    (0xbfe384ad339cfcc3, 0x4001d0768bc335eb),
    (0x3fe720d059892bc4, 0x3fe86a9e804c11f7),
    (0xbfe675c92d60f3df, 0x4002ca65e607e01c),
    (0x3fe9c7b9c3a56969, 0x3fe44a3260b28be7),
    (0xbfe685b6014a22c9, 0x4002cffeb3f9fc16),
    (0x3fe5c47ec47e016d, 0x3fea53a6fc2fd0a5),
    (0xbfe314ef3d87ae57, 0x4001ad7081b25534),
    (0xbfe2311ba18cfd93, 0x4001675d7081001d),
    (0x3fed7011d7bc72c9, 0x3fd9c9018c9811f8),
    (0x3fe89b9538c27512, 0x3fe6316cfee65219),
    (0xbfe38806108a16a3, 0x4001d184ec3de6b3),
    (0x3fe69fb3420a4216, 0x3fe92347d829fe6e),
];

const SQRT_GOLDENS: &[(u64, u64)] = &[
    (0x0000000000000000, 0x0000000000000000),
    (0x8000000000000000, 0x8000000000000000),
    (0x3ff0000000000000, 0x3ff0000000000000),
    (0xbff0000000000000, 0x7ff8000000000000),
    (0x400921fb54442d17, 0x3ffc5bf891b4ef6a),
    (0x400921fb54442d18, 0x3ffc5bf891b4ef6a),
    (0x400921fb54442d19, 0x3ffc5bf891b4ef6b),
    (0xc00921fb54442d17, 0x7ff8000000000000),
    (0xc00921fb54442d18, 0x7ff8000000000000),
    (0xc00921fb54442d19, 0x7ff8000000000000),
    (0x0000000000000001, 0x1e60000000000000),
    (0x8000000000000001, 0x7ff8000000000000),
    (0x000fffffffffffff, 0x1fffffffffffffff),
    (0x800fffffffffffff, 0x7ff8000000000000),
    (0x0010000000000000, 0x2000000000000000),
    (0x8010000000000000, 0x7ff8000000000000),
    (0x7fefffffffffffff, 0x5fefffffffffffff),
    (0xffefffffffffffff, 0x7ff8000000000000),
    (0x7ff0000000000000, 0x7ff0000000000000),
    (0xfff0000000000000, 0x7ff8000000000000),
    (0x7ff8000000000000, 0x7ff8000000000000),
    (0xfff8000000000000, 0x7ff8000000000000),
    (0x7ff8000000000042, 0x7ff8000000000000),
    (0xfff8000000000042, 0x7ff8000000000000),
    (0x3fefc62a2b097592, 0x3fefe307f8bd2805),
    (0xbfe746b419466aec, 0x7ff8000000000000),
    (0x3fe64674f98aa19e, 0x3feab2cd7a205a89),
    (0x3fe4eb47b26de7ac, 0x3fe9df7a3dfcc9cd),
    (0xbfe384ad339cfcc3, 0x7ff8000000000000),
    (0x3fe720d059892bc4, 0x3feb346e7bd6123f),
    (0xbfe675c92d60f3df, 0x7ff8000000000000),
    (0x3fe9c7b9c3a56969, 0x3fecb8e329b303f3),
    (0xbfe685b6014a22c9, 0x7ff8000000000000),
    (0x3fe5c47ec47e016d, 0x3fea6478335e6d56),
    (0xbfe314ef3d87ae57, 0x7ff8000000000000),
    (0xbfe2311ba18cfd93, 0x7ff8000000000000),
    (0x3fed7011d7bc72c9, 0x3feeb1316d26bafd),
    (0x3fe89b9538c27512, 0x3fec0fbe8eac8a9d),
    (0xbfe38806108a16a3, 0x7ff8000000000000),
    (0x3fe69fb3420a4216, 0x3feae813c6e88835),
];

const ATAN2_GOLDENS: &[((u64, u64), u64)] = &[
    ((0x0000000000000000, 0x3ff0000000000000), 0x0000000000000000),
    ((0x8000000000000000, 0x3ff0000000000000), 0x8000000000000000),
    ((0x0000000000000000, 0xbff0000000000000), 0x400921fb54442d18),
    ((0x8000000000000000, 0xbff0000000000000), 0xc00921fb54442d18),
    ((0x3ff0000000000000, 0x0000000000000000), 0x3ff921fb54442d18),
    ((0xbff0000000000000, 0x0000000000000000), 0xbff921fb54442d18),
    ((0x3ff0000000000000, 0x3ff0000000000000), 0x3fe921fb54442d18),
    ((0xbff0000000000000, 0x3ff0000000000000), 0xbfe921fb54442d18),
    ((0x3ff0000000000000, 0xbff0000000000000), 0x4002d97c7f3321d2),
    ((0xbff0000000000000, 0xbff0000000000000), 0xc002d97c7f3321d2),
    ((0x400921fb54442d17, 0x3ff0000000000000), 0x3ff433b8a322ddd2),
    ((0xc00921fb54442d19, 0x3ff0000000000000), 0xbff433b8a322ddd3),
    ((0x0000000000000001, 0x3ff0000000000000), 0x0000000000000001),
    ((0x800fffffffffffff, 0xbff0000000000000), 0xc00921fb54442d18),
    ((0x7ff0000000000000, 0x3ff0000000000000), 0x3ff921fb54442d18),
    ((0xfff0000000000000, 0x3ff0000000000000), 0xbff921fb54442d18),
    ((0x3ff0000000000000, 0x7ff0000000000000), 0x0000000000000000),
    ((0xbff0000000000000, 0x7ff0000000000000), 0x8000000000000000),
    ((0x3ff0000000000000, 0xfff0000000000000), 0x400921fb54442d18),
    ((0xbff0000000000000, 0xfff0000000000000), 0xc00921fb54442d18),
    ((0x7ff0000000000000, 0x7ff0000000000000), 0x3fe921fb54442d18),
    ((0xfff0000000000000, 0xfff0000000000000), 0xc002d97c7f3321d2),
    ((0x7ff8000000000042, 0x3ff0000000000000), 0x7ff8000000000000),
    ((0x3ff0000000000000, 0xfff8000000000000), 0x7ff8000000000000),
    ((0x3fe963386bbcbdbe, 0xbfeddd014275f1af), 0x40037f0714410836),
    ((0xbfee6f4516022e3a, 0xbfed44da11164d93), 0xc002b18139e76fd3),
    ((0xbfe8bd9e0026b546, 0xbfe33e0b398e9207), 0xc001dac95c42b23d),
    ((0xbfe858884527647e, 0xbfe1a652466fb661), 0xc00195a93c5716e6),
    ((0x3fe29ff66c3f7ab5, 0xbfe25351e7b5259c), 0x4002c8e49cbad8ac),
    ((0xbfebe4dbdb42867a, 0x3fe10cce3d56253e), 0xbff05aba4ce1034d),
    ((0x3fef2cc0b009922b, 0xbfe8f59bee858b1a), 0x4001f7a4bf7354b9),
    ((0x3fe09cd645e83f9e, 0xbfe42e9c374e6942), 0x40039f8a9d421431),
    ((0x3fe3380b70c5457f, 0xbfe5e5b38e39c58c), 0x40035eb4e9358080),
    ((0x3fea84c71dc31f28, 0x3fe80a3da5972fef), 0x3feab346d870065c),
    ((0x3fe2c670103f0306, 0xbfe020a6392557bf), 0x40023e6da189dcbc),
    ((0x3fe1c7d06eeca760, 0xbfe20c783f563ee9), 0x4002e8d0c6748a13),
    ((0x3fe5499e56df0d66, 0x3fedf6f7b92f5903), 0x3fe3c4153dec2443),
    ((0xbfeb3aded3f6abe5, 0xbfef4d3cbfb52aa2), 0xc00367b9d6d2bdca),
    ((0xbfeb1b615c6b34c9, 0xbfeca2b39070d2a1), 0xc00311a22cb1b230),
    ((0x3fe94a3df070c516, 0x3fed490deed74ce4), 0x3fe6cb542680951c),
];

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    fn ordinary_bits(&mut self) -> u64 {
        let value = self.next();
        (value & 0x8000_0000_0000_0000) | 0x3fe0_0000_0000_0000 | (value & 0x000f_ffff_ffff_ffff)
    }

    fn unit_interval(&mut self) -> f64 {
        (self.next() >> 11) as f64 * f64::from_bits(0x3ca0_0000_0000_0000)
    }
}

fn unary_sweep() -> Vec<u64> {
    let mut inputs = SPECIAL_INPUTS.to_vec();
    let mut random = SplitMix64(SWEEP_SEED);
    inputs.extend((0..SEEDED_CASES).map(|_| random.ordinary_bits()));
    inputs
}

fn atan2_sweep() -> Vec<(u64, u64)> {
    let mut inputs = ATAN2_SPECIAL_INPUTS.to_vec();
    let mut random = SplitMix64(SWEEP_SEED ^ 0xa5a5_a5a5_a5a5_a5a5);
    inputs.extend((0..SEEDED_CASES).map(|_| (random.ordinary_bits(), random.ordinary_bits())));
    inputs
}

fn ordered_bits(value: f64) -> u64 {
    let bits = value.to_bits();
    if bits >> 63 == 0 {
        bits | 0x8000_0000_0000_0000
    } else {
        !bits
    }
}

fn assert_within_ulp(actual: f64, expected: f64, limit: u64, context: &str) {
    if expected.is_nan() {
        assert!(actual.is_nan(), "{context}: expected NaN, got {actual:?}");
        return;
    }
    if actual == expected {
        return;
    }
    let distance = ordered_bits(actual).abs_diff(ordered_bits(expected));
    assert!(
        distance <= limit,
        "{context}: {actual:?} differs from {expected:?} by {distance} ulp"
    );
}

#[test]
fn bit_pattern_goldens_are_frozen() {
    let unary = unary_sweep();
    for (name, kernel, goldens) in [
        ("sin", sin_det as fn(f64) -> f64, SIN_GOLDENS),
        ("cos", cos_det, COS_GOLDENS),
        ("asin", asin_det, ASIN_GOLDENS),
        ("acos", acos_det, ACOS_GOLDENS),
        ("sqrt", sqrt_det, SQRT_GOLDENS),
    ] {
        assert_eq!(unary.len(), goldens.len(), "{name} golden length");
        for (&sweep_bits, &(input_bits, output_bits)) in unary.iter().zip(goldens) {
            assert_eq!(sweep_bits, input_bits, "{name} sweep input changed");
            assert_eq!(
                kernel(f64::from_bits(input_bits)).to_bits(),
                output_bits,
                "{name}({input_bits:#018x}) changed"
            );
        }
    }

    let binary = atan2_sweep();
    assert_eq!(binary.len(), ATAN2_GOLDENS.len(), "atan2 golden length");
    for (&sweep_input, &(golden_input, output_bits)) in binary.iter().zip(ATAN2_GOLDENS) {
        assert_eq!(sweep_input, golden_input, "atan2 sweep input changed");
        let (y_bits, x_bits) = golden_input;
        assert_eq!(
            atan2_det(f64::from_bits(y_bits), f64::from_bits(x_bits)).to_bits(),
            output_bits,
            "atan2({y_bits:#018x}, {x_bits:#018x}) changed"
        );
    }
}

#[test]
fn sanity_envelope_is_within_two_ulp() {
    for input_bits in unary_sweep() {
        let input = f64::from_bits(input_bits);
        assert_within_ulp(sin_det(input), input.sin(), 2, "sin");
        assert_within_ulp(cos_det(input), input.cos(), 2, "cos");
        assert_within_ulp(asin_det(input), input.asin(), 2, "asin");
        assert_within_ulp(acos_det(input), input.acos(), 2, "acos");
        assert_within_ulp(sqrt_det(input), input.sqrt(), 2, "sqrt");
    }
    for (y_bits, x_bits) in atan2_sweep() {
        let y = f64::from_bits(y_bits);
        let x = f64::from_bits(x_bits);
        assert_within_ulp(atan2_det(y, x), y.atan2(x), 2, "atan2");
    }
}

#[test]
fn vec3_round_trip_is_within_one_ulp() {
    let mut coordinates = vec![
        (-FRAC_PI_2, 0.0),
        (FRAC_PI_2, 0.0),
        (0.0, 0.0),
        (-0.0, -0.0),
        (0.0, -PI),
        (0.0, f64::from_bits(PI.to_bits() - 1)),
    ];
    let mut random = SplitMix64(SWEEP_SEED ^ 0x3c6e_f372_fe94_f82b);
    for _ in 0..64 {
        let lat = (2.0 * random.unit_interval() - 1.0) * FRAC_PI_2;
        let lng = (2.0 * random.unit_interval() - 1.0) * PI;
        coordinates.push((lat, lng));
    }

    for (lat, lng) in coordinates {
        let (round_lat, round_lng) = Vec3::from_lat_lng_rad(lat, lng).to_lat_lng_rad();
        assert_within_ulp(round_lat, lat, 1, "latitude round trip");
        assert_within_ulp(round_lng, lng, 1, "longitude round trip");
    }
}

#[test]
fn vec3_operations_are_right_handed_and_normalized() {
    let x = Vec3 {
        x: 1.0,
        y: 0.0,
        z: 0.0,
    };
    let y = Vec3 {
        x: 0.0,
        y: 1.0,
        z: 0.0,
    };
    assert_eq!(x.dot(y).to_bits(), 0.0_f64.to_bits());
    assert_eq!(
        x.cross(y),
        Vec3 {
            x: 0.0,
            y: 0.0,
            z: 1.0
        }
    );

    let normalized = Vec3 {
        x: 3.0,
        y: 4.0,
        z: 0.0,
    }
    .normalize();
    assert_within_ulp(normalized.x, 0.6, 1, "normalized x");
    assert_within_ulp(normalized.y, 0.8, 1, "normalized y");
    assert_eq!(normalized.z.to_bits(), 0.0_f64.to_bits());
}
