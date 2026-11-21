// buzzs-rust-php-project - Copyright (c) 2025-2026 David Buzz <davidbuzz@gmail.com>
// Licensed under the BSD-3-Clause, the license PHP master uses; see LICENSE.
//! End-to-end tests: PHP scripts run on a VM with the `date` extension.
//! Expected outputs come from `/usr/bin/php` (`date.timezone=UTC`), except
//! `sun` whose `sunrise`/`sunset` follow php-src master (upper-limb
//! refraction of -35/60 degrees, changed after 8.3).

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
    let ext = php_ext_date::Date;
    let exts: [&dyn Extension; 2] = [&php_ext_stdstr::StdStr, &ext];
    let opts = VmOptions {
        ini: vec![("date.timezone".into(), "UTC".into())],
    };
    let mut vm = Vm::with_extensions(&opts, Box::new(sink.clone()), &exts);
    vm.run_script(format!("<?php {code}").as_bytes(), "test.php");
    drop(vm);
    let out = sink.0.borrow().clone();
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn functions() {
    assert_eq!(
        run(
            r"date_default_timezone_set('Europe/Amsterdam'); echo date('D, d M Y H:i:s T', 1136073600), ' ', gmdate('c', 0), ' ', strtotime('2005-07-14 22:30:41'), ' ', mktime(0, 0, 0, 1, 1, 2000), ' ', gmmktime(0, 0, 0, 13, 1, 1999), ' ', idate('Y', 0), ' ', var_export(checkdate(2, 29, 2001), true), ' ', date_default_timezone_get();"
        ),
        r"Sun, 01 Jan 2006 01:00:00 CET 1970-01-01T00:00:00+00:00 1121373041 946681200 946684800 1970 false Europe/Amsterdam"
    );
}

#[test]
fn date_parse() {
    assert_eq!(
        run(
            r"print_r(date_parse('2006-12-12 10:00:00.5 +1 week +1 hour')); print_r(date_parse_from_format('j.n.Y H:iP', '6.1.2009 13:00+01:00'));"
        ),
        r"Array
(
    [year] => 2006
    [month] => 12
    [day] => 12
    [hour] => 10
    [minute] => 0
    [second] => 0
    [fraction] => 0.5
    [warning_count] => 0
    [warnings] => Array
        (
        )

    [error_count] => 0
    [errors] => Array
        (
        )

    [is_localtime] => 
    [relative] => Array
        (
            [year] => 0
            [month] => 0
            [day] => 7
            [hour] => 1
            [minute] => 0
            [second] => 0
        )

)
Array
(
    [year] => 2009
    [month] => 1
    [day] => 6
    [hour] => 13
    [minute] => 0
    [second] => 0
    [fraction] => 0
    [warning_count] => 0
    [warnings] => Array
        (
        )

    [error_count] => 0
    [errors] => Array
        (
        )

    [is_localtime] => 1
    [zone_type] => 1
    [zone] => 3600
    [is_dst] => 
)
"
    );
}

#[test]
fn last_errors() {
    assert_eq!(
        run(
            r"var_dump(date_create('2009-02-30 25:00')); print_r(date_get_last_errors()); var_dump(DateTime::createFromFormat('!Y-m-d', '2020-01-01'), DateTime::getLastErrors());"
        ),
        r#"bool(false)
Array
(
    [warning_count] => 1
    [warnings] => Array
        (
            [17] => The parsed date was invalid
        )

    [error_count] => 1
    [errors] => Array
        (
            [11] => Unexpected character
        )

)
object(DateTime)#1 (3) {
  ["date"]=>
  string(26) "2020-01-01 00:00:00.000000"
  ["timezone_type"]=>
  int(3)
  ["timezone"]=>
  string(3) "UTC"
}
bool(false)
"#
    );
}

#[test]
fn datetime() {
    assert_eq!(
        run(
            r#"$d = new DateTime('2010-01-31 10:00:00', new DateTimeZone('Europe/Amsterdam')); $d->modify('+1 month'); echo $d->format(DATE_ATOM), ' ', $d->getOffset(), ' ', $d->getTimestamp(), ' ', $d->getTimezone()->getName(), "\n"; $d->setDate(2020, 2, 29)->setTime(1, 2, 3, 4)->setTimezone(new DateTimeZone('UTC')); var_dump($d); $d->setISODate(2021, 1, 1); echo $d->format('Y-m-d'), "\n"; $d->setTimestamp(0); echo $d->format('c u'), "\n";"#
        ),
        r#"2010-03-03T10:00:00+01:00 3600 1267606800 Europe/Amsterdam
object(DateTime)#1 (3) {
  ["date"]=>
  string(26) "2020-02-29 00:02:03.000004"
  ["timezone_type"]=>
  int(3)
  ["timezone"]=>
  string(3) "UTC"
}
2021-01-04
1970-01-01T00:00:00+00:00 000000
"#
    );
}

#[test]
fn immutable() {
    assert_eq!(
        run(
            r#"$a = new DateTimeImmutable('2000-01-01 00:00:00', new DateTimeZone('UTC')); $b = $a->add(new DateInterval('PT1H')); $c = $b->sub(new DateInterval('P1D')); echo $a->format('c'), ' ', $b->format('c'), ' ', $c->format('c'), "\n"; var_dump($a < $b, $a == new DateTimeImmutable('2000-01-01 00:00:00 UTC'), $a->diff($c)->format('%R%a %d %h'));"#
        ),
        r#"2000-01-01T00:00:00+00:00 2000-01-01T01:00:00+00:00 1999-12-31T01:00:00+00:00
bool(true)
bool(true)
string(7) "-0 0 23"
"#
    );
}

#[test]
fn exceptions() {
    assert_eq!(
        run(
            r#"foreach (['new DateTime("garbage");', '(new DateTime())->modify("garbage");', 'new DateTimeZone("Foo/Bar");', 'new DateInterval("PXD");', 'DateInterval::createFromDateString("2020-01-01");', '(new DateTimeImmutable())->sub(DateInterval::createFromDateString("next weekday"));'] as $c) { try { eval($c); } catch (Throwable $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; } }"#
        ),
        r"DateMalformedStringException: Failed to parse time string (garbage) at position 0 (g): The timezone could not be found in the database
DateMalformedStringException: DateTime::modify(): Failed to parse time string (garbage) at position 0 (g): The timezone could not be found in the database
DateInvalidTimeZoneException: DateTimeZone::__construct(): Unknown or bad timezone (Foo/Bar)
DateMalformedIntervalStringException: Unknown or bad format (PXD)
DateMalformedIntervalStringException: String '2020-01-01' contains non-relative elements

Warning: The return value of method DateTimeImmutable::sub() should either be used or intentionally ignored by casting it as (void), as DateTimeImmutable::sub() does not modify the object itself in test.php(1) : eval()'d code on line 1
DateInvalidOperationException: DateTimeImmutable::sub(): Only non-special relative time specifications are supported for subtraction
"
    );
}

#[test]
fn uninit() {
    assert_eq!(
        run(
            r#"class Q extends DateTime { function __construct() {} } try { (new Q)->format('Y'); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; } class R extends DateTimeZone { function __construct() {} } try { (new R)->getName(); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }"#
        ),
        r"DateObjectError: Object of type Q (inheriting DateTime) has not been correctly initialized by calling parent::__construct() in its constructor
DateObjectError: Object of type R (inheriting DateTimeZone) has not been correctly initialized by calling parent::__construct() in its constructor
"
    );
}

#[test]
fn serialize() {
    assert_eq!(
        run(
            r#"class My extends DateTime { public $x = 1; } $d = new My('2020-02-03 04:05:06.789', new DateTimeZone('+02:00')); $s = serialize($d); echo $s, "\n"; var_dump(unserialize($s)); var_export(new DateTimeZone('Europe/Paris')); echo "\n"; var_dump(DateTime::__set_state(['date' => '2001-01-01 00:00:00.000000', 'timezone_type' => 2, 'timezone' => 'EST'])); try { unserialize('O:8:"DateTime":0:{}'); } catch (Error $e) { echo $e->getMessage(), "\n"; }"#
        ),
        r#"O:2:"My":4:{s:4:"date";s:26:"2020-02-03 04:05:06.789000";s:13:"timezone_type";i:1;s:8:"timezone";s:6:"+02:00";s:1:"x";i:1;}
object(My)#2 (4) {
  ["x"]=>
  int(1)
  ["date"]=>
  string(26) "2020-02-03 04:05:06.789000"
  ["timezone_type"]=>
  int(1)
  ["timezone"]=>
  string(6) "+02:00"
}
\DateTimeZone::__set_state(array(
   'timezone_type' => 3,
   'timezone' => 'Europe/Paris',
))
object(DateTime)#2 (3) {
  ["date"]=>
  string(26) "2001-01-01 00:00:00.000000"
  ["timezone_type"]=>
  int(2)
  ["timezone"]=>
  string(3) "EST"
}
Invalid serialization data for DateTime object
"#
    );
}

#[test]
fn timezone() {
    assert_eq!(
        run(
            r"$tz = new DateTimeZone('Europe/Paris'); print_r($tz->getLocation()); print_r($tz->getTransitions(1700000000, 1720000000)); var_dump(new DateTimeZone('EST'), new DateTimeZone('-05:30'), $tz->getOffset(new DateTime('2020-07-01')), timezone_name_from_abbr('CET'), DateTimeZone::listIdentifiers(DateTimeZone::EUROPE)[0], DateTimeZone::listIdentifiers(DateTimeZone::PER_COUNTRY, 'NL'));"
        ),
        r#"Array
(
    [country_code] => FR
    [latitude] => 48.86666
    [longitude] => 2.33333
    [comments] => 
)
Array
(
    [0] => Array
        (
            [ts] => 1700000000
            [time] => 2023-11-14T22:13:20+00:00
            [offset] => 3600
            [isdst] => 
            [abbr] => CET
        )

    [1] => Array
        (
            [ts] => 1711846800
            [time] => 2024-03-31T01:00:00+00:00
            [offset] => 7200
            [isdst] => 1
            [abbr] => CEST
        )

)
object(DateTimeZone)#2 (2) {
  ["timezone_type"]=>
  int(2)
  ["timezone"]=>
  string(3) "EST"
}
object(DateTimeZone)#3 (2) {
  ["timezone_type"]=>
  int(1)
  ["timezone"]=>
  string(6) "-05:30"
}
int(7200)
string(13) "Europe/Berlin"
string(16) "Europe/Amsterdam"
array(1) {
  [0]=>
  string(16) "Europe/Amsterdam"
}
"#
    );
}

#[test]
fn interval() {
    assert_eq!(
        run(
            r#"$i = new DateInterval('P1Y2M3DT4H5M6S'); var_dump($i); $i->d = '5'; $i->f = 0.25; $i->invert = 1; var_dump($i->d, $i->f, isset($i->m), empty($i->m), (array) $i); echo $i->format('%R %y %M %D %h %I %S %f %F %a %%'), "\n"; var_dump(DateInterval::createFromDateString('next weekday'), unserialize(serialize($i)));"#
        ),
        r#"object(DateInterval)#1 (10) {
  ["y"]=>
  int(1)
  ["m"]=>
  int(2)
  ["d"]=>
  int(3)
  ["h"]=>
  int(4)
  ["i"]=>
  int(5)
  ["s"]=>
  int(6)
  ["f"]=>
  float(0)
  ["invert"]=>
  int(0)
  ["days"]=>
  bool(false)
  ["from_string"]=>
  bool(false)
}
int(5)
float(0.25)
bool(true)
bool(false)
array(10) {
  ["y"]=>
  int(1)
  ["m"]=>
  int(2)
  ["d"]=>
  int(5)
  ["h"]=>
  int(4)
  ["i"]=>
  int(5)
  ["s"]=>
  int(6)
  ["f"]=>
  float(0.25)
  ["invert"]=>
  int(1)
  ["days"]=>
  bool(false)
  ["from_string"]=>
  bool(false)
}
- 1 02 05 4 05 06 250000 250000 (unknown) %
object(DateInterval)#2 (2) {
  ["from_string"]=>
  bool(true)
  ["date_string"]=>
  string(12) "next weekday"
}
object(DateInterval)#3 (10) {
  ["y"]=>
  int(1)
  ["m"]=>
  int(2)
  ["d"]=>
  int(5)
  ["h"]=>
  int(4)
  ["i"]=>
  int(5)
  ["s"]=>
  int(6)
  ["f"]=>
  float(0.25)
  ["invert"]=>
  int(1)
  ["days"]=>
  bool(false)
  ["from_string"]=>
  bool(false)
}
"#
    );
}

#[test]
fn period() {
    assert_eq!(
        run(
            r#"$p = new DatePeriod(new DateTime('2020-01-01'), new DateInterval('P1W'), new DateTime('2020-02-01'), DatePeriod::EXCLUDE_START_DATE | DatePeriod::INCLUDE_END_DATE); foreach ($p as $k => $d) echo $k, ' ', get_class($d), ' ', $d->format('Y-m-d'), "\n"; var_dump($p->getStartDate()->format('c'), $p->getEndDate()->format('c'), $p->getDateInterval()->d, $p->getRecurrences(), $p->recurrences, $p->include_end_date); $q = DatePeriod::createFromISO8601String('R2/2012-07-01T00:00:00Z/P7D'); foreach ($q as $d) echo get_class($d), ' ', $d->format('c'), "\n"; try { $q->start = null; } catch (Error $e) { echo $e->getMessage(), "\n"; }"#
        ),
        r#"0 DateTime 2020-01-08
1 DateTime 2020-01-15
2 DateTime 2020-01-22
3 DateTime 2020-01-29
string(25) "2020-01-01T00:00:00+00:00"
string(25) "2020-02-01T00:00:00+00:00"
int(7)
NULL
int(1)
bool(true)
DateTimeImmutable 2012-07-01T00:00:00+00:00
DateTimeImmutable 2012-07-08T00:00:00+00:00
DateTimeImmutable 2012-07-15T00:00:00+00:00
Cannot modify readonly property DatePeriod::$start
"#
    );
}

#[test]
fn sun() {
    assert_eq!(
        run(
            r"var_dump(date_sun_info(1700000000, 52.0, 4.0), @date_sunrise(1700000000, SUNFUNCS_RET_STRING, 52.0, 4.0, 90.83, 1));"
        ),
        r#"array(9) {
  ["sunrise"]=>
  int(1699945301)
  ["sunset"]=>
  int(1699977304)
  ["transit"]=>
  int(1699961302)
  ["civil_twilight_begin"]=>
  int(1699943073)
  ["civil_twilight_end"]=>
  int(1699979532)
  ["nautical_twilight_begin"]=>
  int(1699940609)
  ["nautical_twilight_end"]=>
  int(1699981996)
  ["astronomical_twilight_begin"]=>
  int(1699938228)
  ["astronomical_twilight_end"]=>
  int(1699984377)
}
string(5) "07:59"
"#
    );
}

/// `Time\Duration` (php-src master only; expectations from its phpt files).
#[test]
fn duration() {
    assert_eq!(
        run(r#"$d = Time\Duration::fromSeconds(1, 2)->negate();
            echo serialize($d), ' ', Time\Duration::compare($d, Time\Duration::fromMilliseconds(1500)), ' ';
            $e = $d->add(Time\Duration::fromMinutes(1));
            echo $e->seconds, '.', $e->nanoseconds, ' ', var_export($e->negative, true);
            var_dump(Time\Duration::fromSeconds(5) === Time\Duration::fromSeconds(5));
            try { Time\Duration::fromSeconds(-1); } catch (ValueError $x) { echo $x->getMessage(), "\n"; }
            try { Time\Duration::fromIso8601DurationString('P1D'); } catch (Exception $x) { echo get_class($x), ': ', $x->getMessage(), "\n"; }
            try { $d->divideBy(0); } catch (Error $x) { echo get_class($x), ': ', $x->getMessage(), "\n"; }"#),
        "O:13:\"Time\\Duration\":3:{s:7:\"seconds\";i:1;s:11:\"nanoseconds\";i:2;s:8:\"negative\";b:1;} -1 58.999999998 falsebool(true)\n\
         Time\\Duration::fromSeconds(): Argument #1 ($seconds) must be greater than or equal to 0\n\
         Time\\TimeException: The ISO 8601 duration string may only contain the time (T) aspect\n\
         DivisionByZeroError: Division by zero\n"
    );
}

#[test]
fn anonymous_period_dynamic_property_diagnostics() {
    assert_eq!(
        run(r#"$period = new class(new DateTime('2000-01-01'), new DateInterval('P1D'), 2) extends DatePeriod { public $extra; };
            set_error_handler(function ($level, $message) { echo $message, "\n"; });
            $period->extra = [];
            $period->extra[] = 1;
            $period->dynamic1 = 1;
            $period->dynamic1 = 2;
            $period->dynamic2 = [];
            $period->dynamic2[] = 3;
            $period->dynamic3[] = 4;
            var_dump($period->extra[0], $period->dynamic1, $period->dynamic2[0], $period->dynamic3[0]);
        "#),
        "Creation of dynamic property DatePeriod@anonymous\0test.php:1$0::$dynamic1 is deprecated\n\
         Creation of dynamic property DatePeriod@anonymous\0test.php:1$0::$dynamic2 is deprecated\n\
         Creation of dynamic property DatePeriod@anonymous\0test.php:1$0::$dynamic3 is deprecated\n\
         int(1)\nint(2)\nint(3)\nint(4)\n"
    );
}

#[test]
fn duration_readonly_clone_diagnostics() {
    assert_eq!(
        run(r#"
            $d = Time\Duration::fromSeconds(1, 2);
            foreach ([$d, clone $d] as $value) {
                foreach (['seconds' => 3, 'nanoseconds' => 4, 'negative' => true] as $name => $replacement) {
                    try { $value->$name = $replacement; } catch (Throwable $e) {
                        echo $e::class, ': ', $e->getMessage(), "\n";
                    }
                    foreach (['setValue', 'setRawValue', 'setRawValueWithoutLazyInitialization'] as $method) {
                        try { (new ReflectionProperty($value, $name))->$method($value, $replacement); } catch (Throwable $e) {
                            echo $e::class, ': ', $e->getMessage(), "\n";
                        }
                    }
                    try { clone($value, [$name => $replacement]); } catch (Throwable $e) {
                        echo $e::class, ': ', $e->getMessage(), "\n";
                    }
                    var_dump((new ReflectionProperty($value, $name))->isWritable(null, $value));
                }
                echo $value->seconds, ' ', $value->nanoseconds, ' ', (int) $value->negative, "\n";
            }
            try { (new ReflectionMethod($d, '__construct'))->invoke($d); } catch (Throwable $e) {
                echo $e::class, ': ', $e->getMessage(), "\n";
            }
            echo $d->seconds, ' ', $d->nanoseconds, ' ', (int) $d->negative, "\n";
        "#),
        (0..2)
            .map(|_| {
                ["seconds", "nanoseconds", "negative"]
                    .map(|name| {
                        let readonly = format!("Error: Cannot modify readonly property Time\\Duration::${name}\n");
                        format!(
                            "{}Error: Cannot modify protected(set) readonly property Time\\Duration::${name} from global scope\nbool(false)\n",
                            readonly.repeat(4)
                        )
                    })
                    .concat()
                    + "1 2 0\n"
            })
            .collect::<String>()
            + "Error: Cannot directly construct Time\\Duration, use Time\\Duration::from*() methods instead\n1 2 0\n"
    );
}

#[test]
fn nul_parser_diagnostics() {
    assert_eq!(
        run(r#"
            $s = "2000-01-01\0bad";
            foreach (['DateTime', 'DateTimeImmutable'] as $class) {
                try { new $class($s); } catch (Throwable $e) {
                    echo $e->getMessage(), "\n";
                }
                $d = new $class('2000-01-01 12:34:56.789012', new DateTimeZone('Europe/Paris'));
                try { $d->modify($s); } catch (Throwable $e) {
                    echo $e->getMessage(), "\n";
                }
                echo $d->format('Y-m-d H:i:s.u e'), "\n";
                $errors = $class::getLastErrors();
                echo $errors['error_count'], ' ';
                foreach ($errors['errors'] as $position => $message) { echo $position; }
                echo "\n";
            }
            try { new DateInterval("P\0tail"); } catch (Throwable $e) {
                echo $e->getMessage(), "\n";
            }
            foreach (["bad\0tail", "P1D\0"] as $s) {
                try { DatePeriod::createFromISO8601String($s); } catch (Throwable $e) {
                    echo $e->getMessage(), "\n";
                }
            }
        "#),
        "Failed to parse time string (2000-01-01) at position 11 (b): The timezone could not be found in the database\n\
         DateTime::modify(): Failed to parse time string (2000-01-01) at position 11 (b): The timezone could not be found in the database\n\
         2000-01-01 12:34:56.789012 Europe/Paris\n\
         1 11\n\
         Failed to parse time string (2000-01-01) at position 11 (b): The timezone could not be found in the database\n\
         \nWarning: The return value of method DateTimeImmutable::modify() should either be used or intentionally ignored by casting it as (void), as DateTimeImmutable::modify() does not modify the object itself in test.php on line 8\n\
         DateTimeImmutable::modify(): Failed to parse time string (2000-01-01) at position 11 (b): The timezone could not be found in the database\n\
         2000-01-01 12:34:56.789012 Europe/Paris\n\
         1 11\n\
         Unknown or bad format (P)\n\
         Unknown or bad format (bad)\n\
         DatePeriod::createFromISO8601String(): ISO interval must contain a start date, \"P1D\" given\n"
    );
}

// Master's %pS messages are binary-safe, unlike the older installed oracle.
#[test]
fn binary_safe_relative_interval_diagnostics() {
    assert_eq!(
        run(r#"
            try { DateInterval::createFromDateString("bad\0tail"); } catch (Throwable $e) {
                echo str_replace("\0", '<NUL>', $e->getMessage()), "\n";
            }
            try { DateInterval::createFromDateString("2000-01-01\0"); } catch (Throwable $e) {
                echo str_replace("\0", '<NUL>', $e->getMessage()), "\n";
            }
            $i = new DateInterval('P1D');
            try { $i->__unserialize(['date_string' => "bad\0tail"]); } catch (Throwable $e) {
                echo str_replace("\0", '<NUL>', $e->getMessage()), "\n";
            }
        "#),
        "Unknown or bad format (bad<NUL>tail) at position 0 (b): The timezone could not be found in the database\n\
         String '2000-01-01<NUL>' contains non-relative elements\n\
         Unknown or bad format (bad<NUL>tail) at position 0 (b) while unserializing: The timezone could not be found in the database\n"
    );
}

#[test]
fn restored_interval_signed_days_format() {
    assert_eq!(
        run(r#"
            foreach (["\v-9223372036854775808tail", "-9223372036854775808",
                      "-9223372036854775809", "9223372036854775807",
                      "9223372036854775808", "--1", "+12.5", "\0-12",
                      "1e3", "-9999999"] as $s) {
                $i = DateInterval::__set_state(['days' => $s]);
                echo $i->days === false ? 'false' : $i->days, ' ', $i->format('%a'), "\n";
                $j = new DateInterval('P1D');
                $j->__unserialize(['days' => $s]);
                echo $j->days === false ? 'false' : $j->days, ' ', date_interval_format($j, '%a'), "\n";
            }
        "#),
        "-9223372036854775808 0\n-9223372036854775808 0\n\
         -9223372036854775808 0\n-9223372036854775808 0\n\
         -9223372036854775808 0\n-9223372036854775808 0\n\
         9223372036854775807 -1\n9223372036854775807 -1\n\
         9223372036854775807 -1\n9223372036854775807 -1\n\
         0 0\n0 0\n12 12\n12 12\n0 0\n0 0\n1 1\n1 1\n\
         false (unknown)\nfalse (unknown)\n"
    );
}

#[test]
fn restored_interval_unsigned_relative_arithmetic() {
    assert_eq!(
        run(r#"
            $base = ['y' => 0, 'm' => 0, 'd' => 0, 'h' => 0, 'i' => 0, 's' => 0];
            foreach ([
                ['special_type' => 4294967297, 'special_amount' => 1, 'have_special_relative' => 1],
                ['special_type' => -4294967295, 'special_amount' => 1, 'have_special_relative' => 1],
                ['special_type' => 1, 'special_amount' => 1, 'have_special_relative' => 4294967296],
                ['weekday' => 1, 'weekday_behavior' => 1, 'have_weekday_relative' => 4294967296],
                ['special_type' => 1, 'special_amount' => 1, 'have_special_relative' => 4294967297]
            ] as $h) {
                $i = DateInterval::__set_state($h + $base);
                $d = new DateTimeImmutable('2024-01-05 12:34:56', new DateTimeZone('UTC'));
                echo $d->add($i)->format('Y-m-d H:i:s'), ' ';
                try { echo $d->sub($i)->format('Y-m-d H:i:s'); } catch (Throwable $e) {
                    echo get_class($e), ': ', $e->getMessage();
                }
                echo "\n";
            }
        "#),
        "2024-01-08 12:34:56 DateInvalidOperationException: DateTimeImmutable::sub(): Only non-special relative time specifications are supported for subtraction\n\
         2024-01-08 12:34:56 DateInvalidOperationException: DateTimeImmutable::sub(): Only non-special relative time specifications are supported for subtraction\n\
         2024-01-05 12:34:56 2024-01-05 12:34:56\n\
         2024-01-05 12:34:56 2024-01-05 12:34:56\n\
         2024-01-08 12:34:56 DateInvalidOperationException: DateTimeImmutable::sub(): Only non-special relative time specifications are supported for subtraction\n"
    );
}

#[test]
fn date_period_get_object_vars_native_properties() {
    assert_eq!(
        run(r#"
            class MyDatePeriod extends DatePeriod {
                public int $prop = 3;
                public function __debugInfo() { throw new Error('debug info must not run'); }
            }
            $p = MyDatePeriod::createFromISO8601String('R4/2012-07-01T00:00:00Z/P7D');
            foreach (get_object_vars($p) as $name => $value) echo $name, "\n";
            $vars = get_object_vars($p);
            echo get_class($vars['start']), ' ', $vars['start']->format('Y-m-d H:i:s.u'), "\n";
            var_dump($vars['current'], $vars['end'], $vars['interval']->d,
                     $vars['recurrences'], $vars['include_start_date'], $vars['include_end_date']);
            $vars['start']->modify('+1 day');
            $vars['interval']->d = 99;
            echo $p->start->format('Y-m-d'), ' ', $p->interval->d, "\n";
            foreach ($p as $date) { break; }
            echo get_object_vars($p)['current']->format('Y-m-d'), "\n";
        "#),
        "prop\nstart\ncurrent\nend\ninterval\nrecurrences\ninclude_start_date\ninclude_end_date\n\
         DateTimeImmutable 2012-07-01 00:00:00.000000\n\
         NULL\nNULL\nint(7)\nint(5)\nbool(true)\nbool(false)\n\
         \nWarning: The return value of method DateTimeImmutable::modify() should either be used or intentionally ignored by casting it as (void), as DateTimeImmutable::modify() does not modify the object itself in test.php on line 12\n\
         2012-07-01 7\n2012-07-01\n"
    );
}

#[test]
fn date_period_get_object_vars_visibility_and_uninitialized_state() {
    assert_eq!(
        run(r#"
            class PrivatePeriod extends DatePeriod {
                public $visible = 1;
                protected $guarded = 2;
                private $hidden = 3;
                public int $unset;
                function names() {
                    foreach (get_object_vars($this) as $name => $value) echo $name, "\n";
                }
            }
            $p = PrivatePeriod::createFromISO8601String('R1/2012-07-01T00:00:00Z/P1D');
            foreach (get_object_vars($p) as $name => $value) echo $name, "\n";
            echo "inside\n";
            $p->names();
            class UninitializedPeriod extends DatePeriod {
                public $visible = 1;
                function __construct() {}
            }
            echo "uninitialized\n";
            foreach (get_object_vars(new UninitializedPeriod) as $name => $value) echo $name, "\n";
        "#),
        "visible\nstart\ncurrent\nend\ninterval\nrecurrences\ninclude_start_date\ninclude_end_date\n\
         inside\nvisible\nguarded\nhidden\nstart\ncurrent\nend\ninterval\nrecurrences\ninclude_start_date\ninclude_end_date\n\
         uninitialized\nvisible\n"
    );
}
