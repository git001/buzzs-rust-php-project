// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! Reference values from php-src's ext/random tests and from the PHP oracle.

#![expect(clippy::disallowed_methods, reason = "test code")]

use php_random::engine::generated_from_bytes;
use php_random::legacy::format_uniqid;
use php_random::state::{StateArray, StateValue};
use php_random::{
    Engine, IntervalBoundary, Mt19937, Mt19937Mode, NativeEngine, PcgOneseq128XslRr64, RandomError,
    RandomGlobals, Randomizer, Seed, UserEngine, Xoshiro256StarStar,
};
use std::fmt::Write;

fn hex(b: &[u8]) -> String {
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

/// PHP `serialize()` of an engine object (members are always empty here).
fn php_serialize(class: &str, state: &[StateValue]) -> String {
    let mut s = format!(
        "O:{}:\"{class}\":2:{{i:0;a:0:{{}}i:1;a:{}:{{",
        class.len(),
        state.len()
    );
    for (i, v) in state.iter().enumerate() {
        let _ = write!(s, "i:{i};");
        match v {
            StateValue::Int(n) => {
                let _ = write!(s, "i:{n};");
            }
            StateValue::Str(b) => {
                let _ = write!(s, "s:{}:\"{}\";", b.len(), String::from_utf8_lossy(b));
            }
            StateValue::Other => s.push_str("N;"),
        }
    }
    s.push_str("}}");
    s
}

#[test]
fn mt19937_reference_value() {
    // ext/random/tests/02_engine/mt19937_value.phpt
    let mut e = Mt19937::construct(Some(1234), 0).unwrap();
    for _ in 0..10_000 {
        e.generate().unwrap();
    }
    assert_eq!(hex(&e.generate_bytes().unwrap()), "60fe95d9");
}

#[test]
fn pcg_reference_value() {
    // ext/random/tests/02_engine/pcgoneseq128xslrr64_value.phpt
    let mut e = PcgOneseq128XslRr64::construct(Seed::Int(1234)).unwrap();
    for _ in 0..10_000 {
        e.generate().unwrap();
    }
    e.jump(1_234_567).unwrap();
    assert_eq!(hex(&e.generate_bytes().unwrap()), "b88e2a0f23720a1d");
}

#[test]
fn xoshiro_reference_value() {
    // ext/random/tests/02_engine/xoshiro256starstar_value.phpt
    let mut e = Xoshiro256StarStar::construct(Seed::Int(1234)).unwrap();
    for _ in 0..10_000 {
        e.generate().unwrap();
    }
    e.jump();
    e.jump_long();
    assert_eq!(hex(&e.generate_bytes().unwrap()), "1f197e9ca7969123");
}

#[test]
fn string_seeds() {
    let mut p = PcgOneseq128XslRr64::construct(Seed::Bytes(b"0123456789abcdef")).unwrap();
    assert_eq!(hex(&p.generate_bytes().unwrap()), "33ab451afc58b814");
    let seed = b"ab".repeat(16);
    let mut x = Xoshiro256StarStar::construct(Seed::Bytes(&seed)).unwrap();
    assert_eq!(hex(&x.generate_bytes().unwrap()), "9d8a258f258f258f");
}

#[test]
fn mt_rand_reference_values() {
    // ext/random/tests/01_functions/mt_rand_value.phpt
    let mut g = RandomGlobals::new();
    assert_eq!(
        g.mt_srand(Some(12_345_678), 1),
        Some("The MT_RAND_PHP variant of Mt19937 is deprecated")
    );
    let first: Vec<i64> = (0..16).map(|_| g.mt_rand()).collect();
    assert_eq!(
        first,
        [
            1_614_640_687,
            1_711_027_313,
            857_485_497,
            688_176_834,
            1_386_682_158,
            412_773_096,
            813_703_253,
            898_651_287,
            2_087_374_214,
            1_382_556_330,
            1_640_700_129,
            1_863_374_167,
            1_324_097_651,
            1_923_803_667,
            676_334_965,
            853_386_222
        ]
    );
    let x = (0..1024).fold(0, |x, _| x ^ g.mt_rand());
    assert_eq!(x, 1_571_178_311);

    assert_eq!(g.mt_srand(Some(12_345_678), 0), None);
    let first: Vec<i64> = (0..16).map(|_| g.mt_rand()).collect();
    assert_eq!(
        first,
        [
            527_860_569,
            1_711_027_313,
            1_280_820_687,
            688_176_834,
            770_499_160,
            412_773_096,
            813_703_253,
            898_651_287,
            52_508_912,
            757_323_740,
            511_765_911,
            274_407_457,
            833_082_629,
            1_923_803_667,
            1_461_450_755,
            1_301_698_200
        ]
    );
    let x = (0..1024).fold(0, |x, _| x ^ g.mt_rand());
    assert_eq!(x, 1_612_214_270);
}

#[test]
fn oracle_spot_checks() {
    // php -r 'mt_srand(42); echo mt_rand(), " ", mt_rand(1,100), ...'
    let mut g = RandomGlobals::new();
    g.mt_srand(Some(42), 0);
    assert_eq!(g.mt_rand(), 804_318_771);
    assert_eq!(g.mt_rand_range(1, 100), Ok(68));
    assert_eq!(g.mt_rand_range(-5, 5), Ok(4));
    assert_eq!(g.mt_rand_range(0, i64::MAX), Ok(4_279_532_807_823_660_302));
    assert_eq!(
        g.mt_rand_range(i64::MIN, i64::MAX),
        Ok(1_819_927_850_260_223_047)
    );
    g.mt_srand(Some(42), 1);
    assert_eq!(g.mt_rand(), 1_354_439_493);
    assert_eq!(g.mt_rand_range(1, 100), Ok(80));
    assert_eq!(g.mt_rand_range(-5, 5), Ok(5));
    assert_eq!(g.mt_rand_range(0, i64::MAX), Ok(7_507_852_358_122_995_712));
    g.mt_srand(Some(3), 0);
    assert_eq!(g.mt_rand(), 1_182_829_493);
    assert_eq!(g.rand_range(10, 1), 9);

    let mut r = Randomizer::new(Mt19937::new(1, Mt19937Mode::Mt19937));
    assert_eq!(r.get_int(0, 10), Ok(0));
    assert_eq!(r.get_int(i64::MIN, i64::MAX), Ok(4_064_269_471_349_965_035));
    assert_eq!(r.next_int(), Ok(2_002_651_684));
}

#[test]
fn serialization_formats() {
    let p = PcgOneseq128XslRr64::from_int(1);
    assert_eq!(
        php_serialize("Random\\Engine\\PcgOneseq128XslRr64", &p.serialize_state()),
        r#"O:33:"Random\Engine\PcgOneseq128XslRr64":2:{i:0;a:0:{}i:1;a:2:{i:0;s:16:"9cd108b9ceabd26b";i:1;s:16:"df3b50d88069a5ef";}}"#
    );
    let x = Xoshiro256StarStar::from_seed64(1);
    assert_eq!(
        php_serialize("Random\\Engine\\Xoshiro256StarStar", &x.serialize_state()),
        r#"O:32:"Random\Engine\Xoshiro256StarStar":2:{i:0;a:0:{}i:1;a:4:{i:0;s:16:"c15c0289ec2d0a91";i:1;s:16:"67ec8e65a18debbe";i:2;s:16:"5e5532fbeea293f8";i:3;s:16:"0bc942ee9086c171";}}"#
    );
    let m = Mt19937::new(1, Mt19937Mode::Mt19937);
    let s = php_serialize("Random\\Engine\\Mt19937", &m.serialize_state());
    assert_eq!(s.len(), 13069);
    assert!(s.starts_with(
        r#"O:21:"Random\Engine\Mt19937":2:{i:0;a:0:{}i:1;a:626:{i:0;s:8:"9a7ab49c";i:1;s:8:"0891d1b1";i:2;s:8:"1717a028";"#
    ));
    assert!(s.ends_with(r#"2;s:8:"e7223f16";i:623;s:8:"5923f806";i:624;i:0;i:625;i:0;}}"#));
    // Xoshiro256StarStar serialize test of php-src.
    let mut x = Xoshiro256StarStar::from_seed64(1234);
    for _ in 0..10_000 {
        x.next_u64();
    }
    x.jump();
    x.jump_long();
    x.next_u64();
    let st = x.serialize_state();
    let s = php_serialize("Random\\Engine\\Xoshiro256StarStar", &st);
    assert!(s.contains("a:4:{"));
}

#[test]
fn serialization_roundtrip_and_errors() {
    let mut m = Mt19937::new(77, Mt19937Mode::Php);
    for _ in 0..100 {
        m.next_u32();
    }
    let st = StateArray::from_list(m.serialize_state());
    let mut m2 = Mt19937::new(0, Mt19937Mode::Mt19937);
    assert!(m2.unserialize_state(&st));
    assert_eq!(m, m2);
    assert_eq!(m.next_u32(), m2.next_u32());

    // Wrong element count, malformed hex, out-of-range count, bad mode.
    let mut base = m.serialize_state();
    let mut bad = base.clone();
    bad.pop();
    assert!(!m2.unserialize_state(&StateArray::from_list(bad)));
    let mut bad = base.clone();
    bad[0] = StateValue::str("5aa6b98");
    assert!(!m2.unserialize_state(&StateArray::from_list(bad)));
    let mut bad = base.clone();
    bad[0] = StateValue::str("5aa6b98z");
    assert!(!m2.unserialize_state(&StateArray::from_list(bad)));
    let mut bad = base.clone();
    bad[624] = StateValue::Int(625);
    assert!(!m2.unserialize_state(&StateArray::from_list(bad)));
    let mut bad = base.clone();
    bad[624] = StateValue::str("1");
    assert!(!m2.unserialize_state(&StateArray::from_list(bad)));
    let mut bad = base.clone();
    bad[625] = StateValue::Int(2);
    assert!(!m2.unserialize_state(&StateArray::from_list(bad)));
    // The count is truncated to 32 bits before the range check, like C.
    base[624] = StateValue::Int((1i64 << 32) + 5);
    assert!(m2.unserialize_state(&StateArray::from_list(base)));

    let mut x = Xoshiro256StarStar::from_seed64(5);
    let zero = StateArray::from_list((0..4).map(|_| StateValue::str("0000000000000000")));
    assert!(!x.unserialize_state(&zero));
    let ok = StateArray::from_list((0..4).map(|_| StateValue::str("0100000000000000")));
    assert!(x.unserialize_state(&ok));
    assert_eq!(x.state(), [1, 1, 1, 1]);

    let mut p = PcgOneseq128XslRr64::from_int(5);
    let mut arr = StateArray::from_list([StateValue::str("0100000000000000")]);
    assert!(!p.unserialize_state(&arr));
    arr.insert_index(1, StateValue::Int(1));
    assert!(!p.unserialize_state(&arr));
    let arr = StateArray::from_list([
        StateValue::str("0100000000000000"),
        StateValue::str("0200000000000000"),
    ]);
    assert!(p.unserialize_state(&arr));
    assert_eq!(p.state(), (1u128 << 64) | 2);

    let mut n = NativeEngine::Pcg(p);
    assert_eq!(
        n.unserialize(3, true, Some(&arr)).unwrap_err().message(),
        "Invalid serialization data for Random\\Engine\\PcgOneseq128XslRr64 object"
    );
    assert!(n.unserialize(2, false, Some(&arr)).is_err());
    assert!(n.unserialize(2, true, None).is_err());
    assert!(n.unserialize(2, true, Some(&arr)).is_ok());
    assert!(NativeEngine::Secure(php_random::Secure)
        .serialize_state()
        .is_none());
}

#[test]
fn constructor_errors() {
    assert_eq!(
        Mt19937::construct(Some(1234), 2).unwrap_err(),
        RandomError::ValueError("Random\\Engine\\Mt19937::__construct(): Argument #2 ($mode) must be either MT_RAND_MT19937 or MT_RAND_PHP".into())
    );
    assert_eq!(
        PcgOneseq128XslRr64::construct(Seed::Bytes(b"short")).unwrap_err().message(),
        "Random\\Engine\\PcgOneseq128XslRr64::__construct(): Argument #1 ($seed) must be a 16 byte (128 bit) string"
    );
    assert_eq!(
        PcgOneseq128XslRr64::from_int(1).jump(-1).unwrap_err().message(),
        "Random\\Engine\\PcgOneseq128XslRr64::jump(): Argument #1 ($advance) must be greater than or equal to 0"
    );
    assert_eq!(
        Xoshiro256StarStar::construct(Seed::Bytes(b"short")).unwrap_err().message(),
        "Random\\Engine\\Xoshiro256StarStar::__construct(): Argument #1 ($seed) must be a 32 byte (256 bit) string"
    );
    assert_eq!(
        Xoshiro256StarStar::construct(Seed::Bytes(&[0; 32])).unwrap_err().message(),
        "Random\\Engine\\Xoshiro256StarStar::__construct(): Argument #1 ($seed) must not consist entirely of NUL bytes"
    );
    assert!(Mt19937::construct(None, 1).is_ok());
    assert!(PcgOneseq128XslRr64::construct(Seed::Random).is_ok());
    assert!(Xoshiro256StarStar::construct(Seed::Random).is_ok());
}

#[test]
fn randomizer_errors() {
    let mut r = Randomizer::new(Xoshiro256StarStar::from_seed64(1));
    let msg = |e: RandomError| e.message().to_owned();
    assert_eq!(
        msg(r.get_int(5, 4).unwrap_err()),
        "Random\\Randomizer::getInt(): Argument #2 ($max) must be greater than or equal to argument #1 ($min)"
    );
    assert_eq!(
        msg(r.get_bytes(0).unwrap_err()),
        "Random\\Randomizer::getBytes(): Argument #1 ($length) must be greater than 0"
    );
    assert_eq!(
        msg(r.get_bytes_from_string(b"", 1).unwrap_err()),
        "Random\\Randomizer::getBytesFromString(): Argument #1 ($string) must not be empty"
    );
    assert_eq!(
        msg(r.get_bytes_from_string(b"a", 0).unwrap_err()),
        "Random\\Randomizer::getBytesFromString(): Argument #2 ($length) must be greater than 0"
    );
    assert_eq!(
        msg(r.pick_array_keys(&php_random::Dense(0), 1).unwrap_err()),
        "Random\\Randomizer::pickArrayKeys(): Argument #1 ($array) must not be empty"
    );
    assert_eq!(
        msg(r.pick_array_keys(&php_random::Dense(2), 3).unwrap_err()),
        "Random\\Randomizer::pickArrayKeys(): Argument #2 ($num) must be between 1 and the number of elements in argument #1 ($array)"
    );
    let b = IntervalBoundary::ClosedOpen;
    assert_eq!(
        msg(r.get_float(f64::NAN, 0.0, b).unwrap_err()),
        "Random\\Randomizer::getFloat(): Argument #1 ($min) must be finite"
    );
    assert_eq!(
        msg(r.get_float(0.0, f64::INFINITY, b).unwrap_err()),
        "Random\\Randomizer::getFloat(): Argument #2 ($max) must be finite"
    );
    assert_eq!(
        msg(r.get_float(0.0, 0.0, b).unwrap_err()),
        "Random\\Randomizer::getFloat(): Argument #2 ($max) must be greater than argument #1 ($min)"
    );
    assert_eq!(
        msg(r
            .get_float(0.0, -0.1, IntervalBoundary::ClosedClosed)
            .unwrap_err()),
        "Random\\Randomizer::getFloat(): Argument #2 ($max) must be greater than or equal to argument #1 ($min)"
    );
    assert_eq!(
        r.get_float(0.0, 0.0, IntervalBoundary::ClosedClosed),
        Ok(0.0)
    );
    assert_eq!(
        r.get_float(1e17, 1e17 + 0.1, IntervalBoundary::ClosedClosed),
        Ok(1e17)
    );
    assert_eq!(
        r.get_float(1.0, 1.0 + f64::EPSILON, IntervalBoundary::ClosedOpen),
        Ok(1.0)
    );
    assert_eq!(
        r.get_float(1.0, 1.0 + f64::EPSILON, IntervalBoundary::OpenClosed),
        Ok(1.0 + f64::EPSILON)
    );
    assert_eq!(
        msg(r
            .get_float(1.0, 1.0 + f64::EPSILON, IntervalBoundary::OpenOpen)
            .unwrap_err()),
        "The given interval is empty, there are no floats between argument #1 ($min) and argument #2 ($max)"
    );
    assert!(php_random::randomizer::validate_unserialize(1, true).is_ok());
    assert_eq!(
        php_random::randomizer::validate_unserialize(2, true)
            .unwrap_err()
            .to_string(),
        "Exception: Invalid serialization data for Random\\Randomizer object"
    );
}

#[test]
fn legacy_errors() {
    let mut g = RandomGlobals::new();
    assert!(!g.is_mt_seeded());
    assert_eq!(
        g.mt_rand_range(2, 1).unwrap_err().message(),
        "mt_rand(): Argument #2 ($max) must be greater than or equal to argument #1 ($min)"
    );
    assert_eq!(
        g.array_rand(&php_random::Dense(0), 1)
            .unwrap_err()
            .message(),
        "array_rand(): Argument #1 ($array) must not be empty"
    );
    // Like PHP, array_rand() seeds the default engine before validating.
    assert!(g.is_mt_seeded());
    assert_eq!(RandomGlobals::mt_getrandmax(), 2_147_483_647);
}

fn broken_engine_checks<F: FnMut() -> php_random::Result<Vec<u8>>>(
    make: impl Fn() -> UserEngine<F>,
    expected: &RandomError,
    next_int_ok: bool,
    get_bytes_ok: bool,
) {
    let big: Vec<u32> = (0..1234).collect();
    let r = || Randomizer::new(make());
    assert_eq!(r().get_int(0, 1234).unwrap_err(), *expected);
    assert_eq!(r().next_int().is_ok(), next_int_ok);
    assert_eq!(r().get_bytes(1).is_ok(), get_bytes_ok);
    assert_eq!(r().shuffle_array(&mut big.clone()).unwrap_err(), *expected);
    assert_eq!(
        r().pick_array_keys(&php_random::Dense(1234), 1)
            .unwrap_err(),
        *expected
    );
    assert_eq!(
        r().pick_array_keys(&php_random::Dense(1234), 10)
            .unwrap_err(),
        *expected
    );
    assert_eq!(r().shuffle_bytes(b"foobar").unwrap_err(), *expected);
    assert_eq!(
        r().get_bytes_from_string(b"123", 10).unwrap_err(),
        *expected
    );
    assert_eq!(
        r().get_bytes_from_string(&[b'a'; 500], 10).unwrap_err(),
        *expected
    );
}

#[test]
fn heavily_biased_engine() {
    // ext/random/tests/03_randomizer/engine_unsafe_biased.phpt
    broken_engine_checks(
        || UserEngine(|| Ok(vec![0xff; 8])),
        &RandomError::too_many_attempts(),
        true,
        true,
    );
    let mut r = Randomizer::new(UserEngine(|| Ok(vec![0xff; 8])));
    assert_eq!(r.get_bytes(1), Ok(vec![0xff]));
    assert_eq!(r.next_int(), Ok(i64::MAX));
}

#[test]
fn empty_string_engine() {
    // ext/random/tests/03_randomizer/engine_unsafe_empty_string.phpt
    broken_engine_checks(
        || UserEngine(|| Ok(Vec::new())),
        &RandomError::BrokenEngine("A random engine must return a non-empty string".into()),
        false,
        false,
    );
}

#[test]
fn throwing_engine_propagates() {
    let mut r = Randomizer::new(UserEngine(|| Err(RandomError::Propagated)));
    assert_eq!(r.get_int(0, 10), Err(RandomError::Propagated));
    assert_eq!(r.next_float(), Err(RandomError::Propagated));
    assert_eq!(r.get_bytes(10), Err(RandomError::Propagated));
}

#[test]
fn shuffle_keeps_partial_progress_on_error() {
    // An engine that works a few times then throws.
    let mut n = 0;
    let mut r = Randomizer::new(UserEngine(move || {
        n += 1;
        if n > 3 {
            Err(RandomError::Propagated)
        } else {
            Ok(vec![1, 0, 0, 0])
        }
    }));
    let mut v: Vec<u32> = (0..10).collect();
    assert_eq!(r.shuffle_array(&mut v), Err(RandomError::Propagated));
    // Draws of 1 for n_left = 9, 8, 7 swap positions 9/8/7 with 1.
    assert_eq!(v, [0, 7, 2, 3, 4, 5, 6, 8, 9, 1]);
}

#[test]
fn generated_from_user_bytes() {
    let g = generated_from_bytes(b"\x01\x02\x03").unwrap();
    assert_eq!((g.value, g.size), (0x0003_0201, 3));
}

#[test]
fn lcg_value_sequences() {
    // Values computed with the C php_combined_lcg() for the given states.
    let cases: [(i32, i32, [u64; 6]); 7] = [
        (0, 0, [0x3fef_ffff_f922_1812; 6]),
        (
            1,
            0,
            [
                0x3ef3_89c0_08ef_3c14,
                0x3fe7_dbc9_fbe9_0301,
                0x3fe4_100f_d66c_a757,
                0x3fca_3817_dcfd_5a25,
                0x3fd5_d29f_8afa_b081,
                0x3fef_b407_2f3f_5a65,
            ],
        ),
        (
            -1_985_229_329,
            19_088_743,
            [
                0x3fe2_4c54_495e_1510,
                0x3fe2_07c7_6f3e_bc34,
                0x3fec_4c73_a5b0_caee,
                0x3fe3_d5e8_6fd2_0f87,
                0x3fdb_7f2b_2c12_eb60,
                0x3fec_f702_21be_c936,
            ],
        ),
        (
            -1,
            -1,
            [
                0x3e9a_5000_0c08_48d3,
                0x3f9a_178b_8bee_77e3,
                0x3fd6_8f9f_ed51_1e0c,
                0x3fe5_575d_0582_5309,
                0x3fee_d159_cf57_b253,
                0x3fea_d341_c2c4_4e72,
            ],
        ),
        (
            i32::MIN,
            i32::MIN,
            [
                0x3f69_ad84_8bbd_fbbf,
                0x3fe3_d495_6811_747f,
                0x3fe3_2370_63c0_72fc,
                0x3fe4_f54c_edd5_7b43,
                0x3fe7_1761_598f_3255,
                0x3fcb_3e4f_4275_429f,
            ],
        ),
        (
            i32::MAX,
            i32::MAX,
            [
                0x3fef_e653_4716_5a77,
                0x3fd9_f84d_db60_2ea6,
                0x3fe8_245f_b50a_342f,
                0x3f88_8410_1b35_f611,
                0x3fce_e7e1_d721_fff8,
                0x3fe4_03ae_1b26_fddd,
            ],
        ),
        (
            1_942_892_530,
            2874,
            [
                0x3fe9_d022_200d_d008,
                0x3fe4_af84_4cf5_9201,
                0x3feb_55f9_c540_1519,
                0x3fe3_7f12_6b2a_5a08,
                0x3fea_2121_acb2_da20,
                0x3fd6_b45d_44e1_eafd,
            ],
        ),
    ];
    for (s1, s2, expected) in cases {
        let mut g = RandomGlobals::new();
        g.seed_combined_lcg(s1, s2);
        for e in expected {
            assert_eq!(g.lcg_value().to_bits(), e, "seed ({s1}, {s2})");
        }
    }
    let mut g = RandomGlobals::new();
    for _ in 0..100 {
        let v = g.lcg_value();
        assert!(v > 0.0 && v < 1.0);
    }
}

#[test]
fn uniqid_format() {
    assert_eq!(
        format_uniqid(b"", 0x6543_210f, 0x000f_423f, None),
        b"6543210ff423f".to_vec()
    );
    assert_eq!(
        format_uniqid(b"pre_", 1, 2, Some(u32::MAX)),
        b"pre_000000010000210.00000000".to_vec()
    );
    assert_eq!(
        format_uniqid(b"", 1, 2, Some(0)),
        b"00000001000020.00000000".to_vec()
    );
    let mut g = RandomGlobals::new();
    let a = g.uniqid(b"", false);
    let b = g.uniqid(b"", false);
    assert_eq!(a.len(), 13);
    assert_ne!(a, b);
    assert_eq!(g.uniqid(b"x", true).len(), 1 + 13 + 10);
}

#[test]
fn engine_kinds() {
    let e = NativeEngine::Mt19937(Box::new(Mt19937::new(1, Mt19937Mode::Php)));
    assert_eq!(e.kind().class_name(), "Random\\Engine\\Mt19937");
    assert!(e.is_mt19937_legacy());
    assert!(!NativeEngine::Pcg(PcgOneseq128XslRr64::from_int(1)).is_mt19937_legacy());
    assert_eq!(IntervalBoundary::default().name(), "ClosedOpen");
}

fn queue_engine(values: Vec<Vec<u8>>) -> UserEngine<impl FnMut() -> php_random::Result<Vec<u8>>> {
    let mut it = values.into_iter();
    UserEngine(move || Ok(it.next().unwrap_or_default()))
}

#[test]
fn get_float_extreme_range() {
    // ext/random/tests/03_randomizer/methods/getFloat_extreme_range.phpt
    let mut r = Randomizer::new(queue_engine(vec![
        b"\x64\xe8\x58\x79\x3e\xf6\x38\x00".to_vec(),
        b"\x65\xe8\x58\x79\x3e\xf6\x38\x00".to_vec(),
        vec![0; 8],
    ]));
    let b = IntervalBoundary::ClosedOpen;
    assert_eq!(
        r.get_float(-1.6e308, 1.6e308, b),
        Ok(-1.599_999_999_999_999_8e308)
    );
    assert_eq!(r.get_float(-1.6e308, 1.6e308, b), Ok(-1.6e308));
    assert_eq!(
        r.get_float(-1.6e308, 1.6e308, b),
        Ok(1.599_999_999_999_999_8e308)
    );
}

#[test]
fn get_float_opposite_signs() {
    // ext/random/tests/03_randomizer/methods/getFloat_opposite_signs.phpt
    let vals = (0x63u8..=0x67)
        .map(|b| vec![b, 0xe8, 0x58, 0x79, 0x3e, 0xf6, 0x38, 0x00])
        .collect();
    let mut r = Randomizer::new(queue_engine(vals));
    // printf("%.17f") output of the test, parsed back (exact round-trip).
    let expected = [
        "-0.78005908680576086",
        "-0.78005908680576097",
        "-0.78005908680576108",
        "-0.78005908680576119",
        "-0.78005908680576130",
    ]
    .map(|s| s.parse::<f64>().unwrap());
    for e in expected {
        assert_eq!(r.get_float(-1.0, 1.0, IntervalBoundary::ClosedOpen), Ok(e));
    }
}

#[test]
fn legacy_mt_rand_php_cases() {
    // ext/random/tests/01_functions/array_rand_mt_rand_php.phpt
    let mut g = RandomGlobals::new();
    g.mt_srand(Some(1), 1);
    let keys: Vec<u32> = (0..3)
        .map(|_| g.array_rand(&php_random::Dense(3), 1).unwrap()[0])
        .collect();
    assert_eq!(keys, [0, 1, 0]);
    // ext/random/tests/01_functions/bug75514.phpt
    g.mt_srand(Some(0), 1);
    assert_eq!(g.mt_rand_range(0, 999_999_999), Ok(448_865_905));
    assert_eq!(g.mt_rand_range(0, 999), Ok(592));
}

#[test]
fn get_int_gh9415() {
    // ext/random/tests/03_randomizer/methods/getInt_gh9415.phpt
    let mut r = Randomizer::new(Mt19937::new(1234, Mt19937Mode::Mt19937));
    assert_eq!(r.get_int(-2_147_483_648, 2_147_483_647), Ok(-1_324_913_873));
    let mut r = Randomizer::new(Mt19937::new(4321, Mt19937Mode::Mt19937));
    assert_eq!(r.get_int(-2_147_483_648, 2_147_483_647), Ok(-1_843_387_587));
}

fn byte_engine() -> UserEngine<impl FnMut() -> php_random::Result<Vec<u8>>> {
    let mut i = 0u8;
    UserEngine(move || {
        i += 1;
        Ok(vec![i])
    })
}

#[test]
fn get_int_expansion() {
    // ext/random/tests/03_randomizer/methods/getInt_expansion_{32,64}.phpt
    let mut r = Randomizer::new(byte_engine());
    let v = r.get_int(0, 0x00FF_FFFF).unwrap();
    assert_eq!(hex(&u32::try_from(v).unwrap().to_le_bytes()), "01020300");
    let mut r = Randomizer::new(byte_engine());
    let v = r.get_int(0, 0x00FF_FFFF_FFFF_FFFF).unwrap();
    assert_eq!(hex(&v.to_le_bytes()), "0102030405060700");
}

#[test]
fn get_bytes_expansion() {
    // ext/random/tests/03_randomizer/methods/getBytes_expansion.phpt
    let mut r = Randomizer::new(queue_engine(
        [
            &b"H"[..],
            b"e",
            b"ll",
            b"o",
            b"abcdefghijklmnopqrstuvwxyz",
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZ",
            b"success",
        ]
        .iter()
        .map(|s| s.to_vec())
        .collect(),
    ));
    assert_eq!(r.get_bytes(5).unwrap(), b"Hello");
    assert_eq!(r.get_bytes(11).unwrap(), b"abcdefghABC");
    assert_eq!(r.get_bytes(7).unwrap(), b"success");
}

#[test]
fn get_bytes_from_string_call_counts() {
    // ext/random/tests/03_randomizer/methods/getBytesFromString_fast_path.phpt
    let all: Vec<u8> = (0..=255).collect();
    let mut i = 1;
    while i <= 256 {
        let mut calls = 0;
        let mut inner = Xoshiro256StarStar::from_seed64(99);
        let mut r = Randomizer::new(UserEngine(|| {
            calls += 1;
            Ok(inner.next_u64().to_le_bytes().to_vec())
        }));
        let out = r.get_bytes_from_string(&all[..i], 20000).unwrap();
        let mut seen = [false; 256];
        for b in out {
            seen[usize::from(b)] = true;
        }
        assert_eq!(calls, 2500, "len {i}");
        assert_eq!(seen.iter().filter(|&&s| s).count(), i);
        i *= 2;
    }
    let mut calls = 0;
    let mut inner = Xoshiro256StarStar::from_seed64(99);
    let doubled = [all.clone(), all].concat();
    let mut r = Randomizer::new(UserEngine(|| {
        calls += 1;
        Ok(inner.next_u64().to_le_bytes().to_vec())
    }));
    r.get_bytes_from_string(&doubled, 20000).unwrap();
    assert_eq!(calls, 20000);
}
