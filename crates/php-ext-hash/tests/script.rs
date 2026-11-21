// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! End-to-end tests: PHP scripts run on a VM with the `hash` extension.
//! Expected values come from php-src (`/usr/bin/php`).

const EXT: php_ext_hash::Hash = php_ext_hash::Hash;

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use php_vm::ext::Extension;
use php_vm::{Vm, VmOptions};

/// A `Write` sink shared with the test.
#[derive(Clone, Default)]
struct Sink(Rc<RefCell<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Runs `code` (without `<?php`) and returns its output.
fn run(code: &str) -> String {
    let sink = Sink::default();
    let ext = EXT;
    let exts: [&dyn Extension; 3] = [
        &php_ext_stdstr::StdStr,
        &ext,
        &php_ext_hash::StandardDigests,
    ];
    let mut vm = Vm::with_extensions(&VmOptions::default(), Box::new(sink.clone()), &exts);
    vm.run_script(format!("<?php {code}").as_bytes(), "test.php");
    drop(vm);
    let out = sink.0.borrow().clone();
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn one_shot_digests() {
    assert_eq!(
        run(r"echo hash('sha256', 'abc'), ' ', hash_hmac('md5', 'data', 'key');"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad 9d5c73ef85594d34ec4438b7c97e51d8"
    );
    assert_eq!(
        run(r"var_dump(md5('x'), sha1('x'), crc32('x'), hash_equals('a', 'a'));"),
        "string(32) \"9dd4e461268c8034f5c8564e155c67a6\"\nstring(40) \"11f6ad8ec52a2984abaafd7c3b516503785c2072\"\nint(2363233923)\nbool(true)\n"
    );
}

#[test]
fn incremental_and_copy() {
    assert_eq!(
        run(
            r"$c = hash_init('sha1'); hash_update($c, 'ab'); $d = hash_copy($c); hash_update($c, 'c');
            echo hash_final($c), ' ', hash_final($d);"
        ),
        "a9993e364706816aba3e25717850c26c9cd0d89d da23614e02469a0d7c7bd1bdab5c9c474b1904dc"
    );
}

#[test]
fn errors() {
    assert_eq!(
        run(r"$c = hash_init('md5'); hash_final($c);
            try { hash_update($c, 'x'); } catch (TypeError $e) { echo $e->getMessage(), '|'; }
            try { hash('nope', ''); } catch (ValueError $e) { echo $e->getMessage(), '|'; }
            try { hash_equals(1, 'a'); } catch (TypeError $e) { echo $e->getMessage(); }"),
        "hash_update(): Argument #1 ($context) must be a valid, non-finalized HashContext|hash(): Argument #1 ($algo) must be a valid hashing algorithm|hash_equals(): Argument #1 ($known_string) must be of type string, int given"
    );
}

#[test]
fn derivation() {
    assert_eq!(
        run(
            r"echo bin2hex(hash_hkdf('sha256', 'key', 8)), ' ', hash_pbkdf2('sha1', 'p', 's', 2, 10);"
        ),
        "80c724c4a65d7956 ea4b66d403"
    );
}

#[test]
fn serialization_round_trip() {
    assert_eq!(
        run(r"$c = hash_init('md5'); hash_update($c, 'abc');
            echo hash_final(unserialize(serialize($c)));"),
        "900150983cd24fb0d6963f7d28e17f72"
    );
}

#[test]
fn debug_info_shows_algo() {
    assert_eq!(
        run(r"var_dump(hash_init('xxh3'));"),
        "object(HashContext)#1 (1) {\n  [\"algo\"]=>\n  string(4) \"xxh3\"\n}\n"
    );
}

#[test]
fn gh16711_constants_in_registration_order() {
    assert_eq!(
        run(r"$r = new ReflectionExtension('HaSh');
            $constants = $r->getConstants();
            define('HASH_USER_REGRESSION', 99);
            $count = 0;
            foreach ($constants as $value) {
                ++$count;
            }
            var_dump($count);
            foreach ($constants as $name => $value) {
                echo $name, '=', $value, PHP_EOL;
            }
            var_dump($constants === get_defined_constants(true)['hash']);
            var_dump($constants === $r->getConstants());
            var_dump($constants === (new ReflectionExtension('hash'))->getConstants());
            var_dump(isset($r->getConstants()['HASH_USER_REGRESSION']));"),
        "int(40)\n\
HASH_HMAC=1\n\
MHASH_CRC32=0\n\
MHASH_MD5=1\n\
MHASH_SHA1=2\n\
MHASH_HAVAL256=3\n\
MHASH_RIPEMD160=5\n\
MHASH_TIGER=7\n\
MHASH_GOST=8\n\
MHASH_CRC32B=9\n\
MHASH_HAVAL224=10\n\
MHASH_HAVAL192=11\n\
MHASH_HAVAL160=12\n\
MHASH_HAVAL128=13\n\
MHASH_TIGER128=14\n\
MHASH_TIGER160=15\n\
MHASH_MD4=16\n\
MHASH_SHA256=17\n\
MHASH_ADLER32=18\n\
MHASH_SHA224=19\n\
MHASH_SHA512=20\n\
MHASH_SHA384=21\n\
MHASH_WHIRLPOOL=22\n\
MHASH_RIPEMD128=23\n\
MHASH_RIPEMD256=24\n\
MHASH_RIPEMD320=25\n\
MHASH_SNEFRU256=27\n\
MHASH_MD2=28\n\
MHASH_FNV132=29\n\
MHASH_FNV1A32=30\n\
MHASH_FNV164=31\n\
MHASH_FNV1A64=32\n\
MHASH_JOAAT=33\n\
MHASH_CRC32C=34\n\
MHASH_MURMUR3A=35\n\
MHASH_MURMUR3C=36\n\
MHASH_MURMUR3F=37\n\
MHASH_XXH32=38\n\
MHASH_XXH64=39\n\
MHASH_XXH3=40\n\
MHASH_XXH128=41\n\
bool(true)\n\
bool(true)\n\
bool(true)\n\
bool(false)\n"
    );
}

#[test]
fn hash_constant_reflection_attribution() {
    assert_eq!(
        run(
            r"$constants = (new ReflectionExtension('hash'))->getConstants();
            $count = 0;
            $valid = true;
            foreach ($constants as $name => $value) {
                ++$count;
                $r = new ReflectionConstant($name);
                $valid = $valid
                    && $r->getName() === $name
                    && $r->getValue() === $value
                    && $r->getExtensionName() === 'hash'
                    && $r->getExtension()->getConstants() === $constants
                    && $r->isDeprecated() === ($name !== 'HASH_HMAC');
            }
            var_dump($valid && $count === 40);"
        ),
        "bool(true)\n"
    );
}
