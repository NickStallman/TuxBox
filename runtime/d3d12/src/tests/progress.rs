use super::*;

fn idle_for(idle_ms: u64) -> Snapshot {
    Snapshot { idle_ms: Some(idle_ms), ..Snapshot::default() }
}

#[test]
fn mode_parses_the_documented_values() {
    assert_eq!(Mode::parse(None), Mode::Auto);
    assert_eq!(Mode::parse(Some("auto")), Mode::Auto);
    assert_eq!(Mode::parse(Some("always")), Mode::Always);
    assert_eq!(Mode::parse(Some("0")), Mode::Off);
    assert_eq!(Mode::parse(Some("off")), Mode::Off);
    assert_eq!(Mode::parse(Some("nonsense")), Mode::Auto);
}

#[test]
fn auto_shows_while_working_and_lingers_then_hides() {
    assert!(!visible(Mode::Auto, &Snapshot::default()));
    let translating = Snapshot { in_flight: 1, ..idle_for(LINGER_MS * 2) };
    let building = Snapshot { building: 1, ..idle_for(LINGER_MS * 2) };
    assert!(visible(Mode::Auto, &translating));
    assert!(visible(Mode::Auto, &building));
    assert!(visible(Mode::Auto, &idle_for(LINGER_MS - 1)));
    assert!(!visible(Mode::Auto, &idle_for(LINGER_MS)));
}

#[test]
fn always_and_off_ignore_activity() {
    assert!(visible(Mode::Always, &Snapshot::default()));
    assert!(!visible(Mode::Off, &Snapshot { building: 9, ..idle_for(0) }));
}

/// A source or program digest made of one repeated character.
fn digest(of: char) -> String {
    std::iter::repeat_n(of, 64).collect()
}

#[test]
fn a_program_is_counted_once_however_many_requests_it_takes() {
    let mut shaders = Shaders::default();
    // Two roots of one program, its linked pair and its narrow-wave variant.
    shaders.sources.insert(digest('a'), digest('p'));
    shaders.sources.insert(digest('b'), digest('p'));
    assert!(shaders.record(&digest('a'), Shader::Translated));
    for key in [digest('b'), format!("{}_{}_{}", digest('b'), digest('c'), digest('d')), format!("{}_w32", digest('a'))] {
        assert!(!shaders.record(&key, Shader::Cached), "{key}");
    }
    assert!(shaders.record(&digest('e'), Shader::Cached));
    assert_eq!(shaders.counts, ShaderCounts { translated: 1, cached: 1, ..ShaderCounts::default() });
}

#[test]
fn a_program_is_unsupported_only_when_no_request_for_it_compiles() {
    let mut shaders = Shaders::default();
    shaders.sources.insert(digest('a'), digest('p'));
    shaders.sources.insert(digest('b'), digest('p'));
    assert!(shaders.record(&digest('a'), Shader::Unsupported));
    assert_eq!(shaders.counts.unsupported, 1);
    assert!(shaders.record(&digest('b'), Shader::Translated));
    // A later pipeline repeats the refused attempt.
    assert!(!shaders.record(&digest('a'), Shader::Unsupported));
    assert_eq!(shaders.counts, ShaderCounts { translated: 1, ..ShaderCounts::default() });
}

#[test]
fn a_later_result_replaces_a_timeout() {
    let mut shaders = Shaders::default();
    assert!(shaders.record("a", Shader::TimedOut));
    assert!(!shaders.record("a", Shader::TimedOut));
    assert!(shaders.record("a", Shader::Unsupported));
    assert!(shaders.record("a", Shader::Translated));
    assert!(!shaders.record("a", Shader::TimedOut));
    assert_eq!(shaders.counts, ShaderCounts { translated: 1, ..ShaderCounts::default() });
}

#[test]
fn the_pack_total_reads_a_number() {
    assert_eq!(parse_total(Some(" 25889 ")), 25_889);
    assert_eq!(parse_total(Some("many")), 0);
    assert_eq!(parse_total(None), 0);
}

#[test]
fn numbers_are_grouped_in_threes() {
    assert_eq!(grouped(0), "0");
    assert_eq!(grouped(999), "999");
    assert_eq!(grouped(1000), "1,000");
    assert_eq!(grouped(1234567), "1,234,567");
}

#[test]
fn lines_describe_the_counts() {
    let text = lines(&Snapshot {
        in_flight: 3,
        building: 2,
        built: 5120,
        failed: 32,
        stand_ins: 1024,
        shaders: ShaderCounts { translated: 898, cached: 336, unsupported: 12, timed_out: 0 },
        idle_ms: Some(0),
        total: 0,
    });
    assert_eq!(text[0], "TuxBox  |  translating 3, building 2");
    assert_eq!(text[1], "1,234 shaders compiled  (898 new)");
    assert_eq!(text[2], "12 unsupported");
    assert_eq!(text[3], "pipelines 5,120 | failed 32 | empty 1,024");
    assert_eq!(lines(&Snapshot::default())[0], "TuxBox  |  idle");
    let of_pack = Snapshot { total: 25_889, shaders: ShaderCounts { cached: 18_818, ..ShaderCounts::default() }, ..Snapshot::default() };
    assert_eq!(lines(&of_pack)[1], "18,818 of 25,889 shaders compiled  (0 new)");
}

#[test]
fn lines_fit_the_panel_with_large_counts() {
    let busy = Snapshot {
        in_flight: 32,
        building: 32,
        built: 99_999,
        failed: 9_999,
        stand_ins: 99_999,
        shaders: ShaderCounts { translated: 99_999, cached: 0, unsupported: 99_999, timed_out: 999 },
        idle_ms: Some(0),
        total: 99_999,
    };
    // 14 px fixed-pitch text across a 380 px panel holds about 49 columns.
    for line in lines(&busy) {
        assert!(line.len() <= 49, "{} columns: {line}", line.len());
    }
}
