//! Configuration facade behavior tests.

use std::fs;
use std::path::Path;

use crate::playback_mode::RepeatMode;
use crate::track::TrackLocation;

use proptest::prelude::*;

use super::*;
use crate::test_support::unique_temp_dir;

fn volume(value: u16) -> VolumePercent {
    VolumePercent::new(value).expect("test volume must be valid")
}

fn gain(value: f32) -> GainDb {
    GainDb::try_from(value).expect("test gain must be a valid step")
}

fn crossfade(value: u16) -> CrossfadeSeconds {
    CrossfadeSeconds::new(value).expect("test crossfade must be valid")
}

fn key(value: &str) -> KeyChord {
    KeyChord::try_from(value).expect("test key must be valid")
}

#[test]
fn for_home_mirrors_the_xdg_layout() {
    let home = Path::new("/home/someone");
    let paths = Paths::for_home(home);

    assert_eq!(paths.config_dir, home.join(".config/harmonium"));
    assert_eq!(paths.data_dir, home.join(".local/share/harmonium"));
    assert_eq!(paths.cache_dir, home.join(".cache/harmonium"));
    assert_eq!(
        paths.playlists_dir(),
        home.join(".local/share/harmonium/playlists")
    );
}

#[test]
fn ensure_dir_creates_nested_directories_and_is_idempotent() {
    let root = unique_temp_dir("config-ensure");

    let target = root.join("a/b/c");
    ensure_dir(&target).expect("first creation succeeds");
    ensure_dir(&target).expect("second creation is a no op");

    assert!(target.is_dir());
}

#[test]
fn border_type_defaults_and_round_trips_as_lowercase_config() {
    assert_eq!(AppConfig::default().ui.border_type, BorderType::Plain);

    for border_type in BorderType::ALL {
        let root = unique_temp_dir("config-border-type");
        let mut config = AppConfig::default();
        config.ui.border_type = border_type;
        config.save(&root).expect("save config");

        let contents = fs::read_to_string(root.join("config.toml")).expect("config file");
        assert!(contents.contains(&format!(
            "border_type = \"{}\"",
            border_type.label().to_lowercase()
        )));
        assert_eq!(AppConfig::load(&root).ui.border_type, border_type);
    }
}

#[test]
fn invalid_or_missing_border_type_falls_back_to_plain_without_losing_ui_defaults() {
    let root = unique_temp_dir("config-border-invalid");
    fs::write(
        root.join("config.toml"),
        "[ui]\nshow_album_art = false\nborder_type = \"wavy\"\n",
    )
    .expect("config fixture");

    let config = AppConfig::load(&root);
    assert_eq!(config.ui.border_type, BorderType::Plain);
    assert!(!config.ui.show_album_art);

    fs::write(root.join("config.toml"), "[ui]\nshow_album_art = false\n").expect("config fixture");
    let config = AppConfig::load(&root);
    assert_eq!(config.ui.border_type, BorderType::Plain);
    assert!(!config.ui.show_album_art);
}

#[test]
fn paths_are_resolvable_on_this_system() {
    // The binary cannot log or persist anything without this holding,
    // so pin it even though the exact location depends on the host env
    let paths = Paths::system().expect("system paths resolve");

    assert!(!paths.config_dir.as_os_str().is_empty());
    assert!(!paths.data_dir.as_os_str().is_empty());
    assert!(!paths.cache_dir.as_os_str().is_empty());
}

#[test]
fn missing_config_file_yields_defaults_without_noise() {
    let root = unique_temp_dir("config-missing");

    let config = AppConfig::load(&root);

    assert_eq!(config, AppConfig::default());
    assert!(config.ui.show_album_art);
    assert_eq!(config.ui.album_art_mode, AlbumArtMode::Auto);
}

#[test]
fn valid_config_file_overrides_the_defaults() {
    let root = unique_temp_dir("config-valid");
    fs::write(
        root.join("config.toml"),
        "[ui]\nshow_album_art = false\nalbum_art_mode = \"unicode\"\n",
    )
    .expect("config fixture");

    let config = AppConfig::load(&root);

    assert!(!config.ui.show_album_art);
    assert_eq!(config.ui.album_art_mode, AlbumArtMode::Unicode);
}

#[test]
fn partial_section_keeps_the_remaining_defaults() {
    let root = unique_temp_dir("config-partial");
    fs::write(root.join("config.toml"), "[ui]\nshow_album_art = false\n").expect("config fixture");

    let config = AppConfig::load(&root);

    assert!(!config.ui.show_album_art);
    assert_eq!(config.ui.album_art_mode, AlbumArtMode::Auto);
}

#[test]
fn invalid_toml_falls_back_to_defaults_without_panicking() {
    let root = unique_temp_dir("config-invalid");
    fs::write(
        root.join("config.toml"),
        "[ui]\nshow_album_art = false\nalbum_art_mode = \"unterminated\n",
    )
    .expect("config fixture");

    let config = AppConfig::load(&root);

    assert_eq!(config, AppConfig::default());
}

#[test]
fn invalid_scalar_only_falls_back_to_its_field() {
    let root = unique_temp_dir("config-scalar-invalid");
    fs::write(
            root.join("config.toml"),
            "[general]\nvolume_percent = \"loud\"\nconfirm_quit = false\n\n[keys]\nquit = 42\nhelp = \"H\"\n",
        )
        .expect("config fixture");

    let config = AppConfig::load(&root);

    assert_eq!(config.general.volume_percent, volume(70));
    assert!(!config.general.confirm_quit);
    assert_eq!(config.keys.quit.as_str(), "q");
    assert_eq!(config.keys.help.as_str(), "H");
}

#[test]
fn invalid_key_chord_only_falls_back_to_its_field() {
    let root = unique_temp_dir("config-invalid-key-chord");
    fs::write(
        root.join("config.toml"),
        "[keys]\nquit = \"not_a_real_key\"\nhelp = \"ctrl+h\"\n",
    )
    .expect("config fixture");

    let config = AppConfig::load(&root);

    assert_eq!(config.keys.quit.as_str(), "q");
    assert_eq!(config.keys.help.as_str(), "ctrl+h");
}

#[test]
fn invalid_art_mode_only_falls_back_to_its_field() {
    let root = unique_temp_dir("config-unknown-mode");
    fs::write(
            root.join("config.toml"),
            "[ui]\nshow_album_art = false\nalbum_art_mode = \"sixel-force\"\ntheme = \"night\"\nartwork_source = \"remote\"\nborder_type = \"rounded\"\n\n[general]\nvolume_percent = 85\n\n[keys]\nquit = \"Q\"\n\n[playback]\nremote_lyrics = true\n",
        )
        .expect("config fixture");

    let config = AppConfig::load(&root);

    assert_eq!(config.ui.album_art_mode, AlbumArtMode::Auto);
    assert!(!config.ui.show_album_art);
    assert_eq!(config.ui.theme, "night");
    assert_eq!(config.ui.artwork_source, ArtworkSource::Remote);
    assert_eq!(config.ui.border_type, BorderType::Rounded);
    assert_eq!(config.general.volume_percent, volume(85));
    assert_eq!(config.keys.quit.as_str(), "Q");
    assert!(config.playback.remote_lyrics);
}

#[test]
fn invalid_nested_setting_only_falls_back_to_its_field() {
    let root = unique_temp_dir("config-nested-invalid");
    fs::write(
            root.join("config.toml"),
            "[general.playlist_columns]\ndisplay_by = \"invalid\"\nmetadata_artist = true\nmetadata_album = true\n\n[general.now_playing_display]\nsort_by = \"invalid\"\nmetadata_title = true\n",
        )
        .expect("config fixture");

    let config = AppConfig::load(&root);

    assert_eq!(config.general.playlist_columns.display_by, SortBy::Filename);
    assert!(config.general.playlist_columns.metadata_artist);
    assert!(config.general.playlist_columns.metadata_album);
    assert_eq!(config.general.now_playing_display.sort_by, SortBy::Filename);
    assert!(config.general.now_playing_display.metadata_title);
}

#[test]
fn every_documented_art_mode_value_is_accepted() {
    for (literal, expected) in [
        ("auto", AlbumArtMode::Auto),
        ("image", AlbumArtMode::Image),
        ("unicode", AlbumArtMode::Unicode),
        ("off", AlbumArtMode::Off),
    ] {
        let root = unique_temp_dir("config-modes");
        fs::write(
            root.join("config.toml"),
            format!("[ui]\nalbum_art_mode = \"{literal}\"\n"),
        )
        .expect("config fixture");

        assert_eq!(AppConfig::load(&root).ui.album_art_mode, expected);
    }
}

#[test]
fn general_section_loads_preferences() {
    let root = unique_temp_dir("config-general");
    fs::write(
        root.join("config.toml"),
        "[general]\nconfirm_quit = false\nvolume_percent = 85\nshow_hidden = true\n",
    )
    .expect("config fixture");

    let config = AppConfig::load(&root);

    // User preferences live in [general] under config.toml.
    assert!(!config.general.confirm_quit);
    assert_eq!(config.general.volume_percent, volume(85));
    assert!(config.general.show_hidden);
}

#[test]
fn log_section_loads_level() {
    let root = unique_temp_dir("config-log");
    fs::write(root.join("config.toml"), "[log]\nlevel = \"debug\"\n").expect("config fixture");

    let config = AppConfig::load(&root);

    assert_eq!(config.log.level, "debug");
}

#[test]
fn log_defaults_to_info() {
    let config = AppConfig::default();
    assert_eq!(config.log.level, "info");
}

#[test]
fn keys_section_loads_custom_bindings() {
    let root = unique_temp_dir("config-keys");
    fs::write(
        root.join("config.toml"),
        "[keys]\nquit = \"Q\"\nplay_pause = \"enter\"\n",
    )
    .expect("config fixture");

    let config = AppConfig::load(&root);

    assert_eq!(config.keys.quit.as_str(), "Q");
    assert_eq!(config.keys.play_pause.as_str(), "enter");
    // Unset keys keep defaults
    assert_eq!(config.keys.next.as_str(), "n");
}

#[test]
fn live_key_binding_fields_round_trip_without_numeric_fallbacks() {
    let mut keys = KeysConfig::default();
    for (index, field) in KeyBindingField::ALL.into_iter().enumerate() {
        let value = format!("ctrl+{index}");
        field.set(&mut keys, value.clone()).unwrap();
        assert_eq!(field.get(&keys), value);
    }

    assert_eq!(
        KeyBindingField::from_index(KeyBindingField::ALL.len()),
        None
    );
    assert_eq!(KeyBindingField::from_index(usize::MAX), None);
    assert_eq!(KeySettingsRow::from_index(KeySettingsRow::ALL.len()), None);
    assert_eq!(KeySettingsRow::Cancel.live_field(), None);
    let live_labels: Vec<&str> = KeyBindingField::ALL
        .into_iter()
        .map(KeyBindingField::label)
        .collect();
    assert!(!live_labels.contains(&"cancel"));
    assert!(!live_labels.contains(&"toggle_artwork"));
}

#[test]
fn key_settings_rows_preserve_the_existing_labels_and_order() {
    assert_eq!(
        KeySettingsRow::ALL.map(KeySettingsRow::label),
        [
            "quit",
            "cancel",
            "play_pause",
            "next",
            "previous",
            "volume_up",
            "volume_down",
            "repeat",
            "shuffle",
            "lyrics",
        ]
    );
}

#[test]
fn missing_state_file_yields_defaults() {
    let root = unique_temp_dir("state-missing");

    let state = PersistedState::load(&root);

    assert_eq!(state, PersistedState::default());
}

#[test]
fn valid_state_file_round_trips() {
    let root = unique_temp_dir("state-valid");
    fs::create_dir_all(&root).expect("dir");
    let original = PersistedState {
        repeat_mode: RepeatMode::Track,
        shuffle: true,
        last_playlist: Some("Rock Favorites".to_string()),
        last_track_path: Some("/music/rock/favorite.flac".to_string()),
        last_track_position_ms: 42_500,
    };

    original.save(&root);
    let contents = fs::read_to_string(root.join(STATE_FILE_NAME)).expect("read state");
    assert!(contents.contains("repeat_mode = \"track\""));
    let loaded = PersistedState::load(&root);

    assert_eq!(loaded.repeat_mode, RepeatMode::Track);
    assert!(loaded.shuffle);
    assert_eq!(loaded.last_playlist.as_deref(), Some("Rock Favorites"));
    assert_eq!(
        loaded.last_track_path.as_deref(),
        Some("/music/rock/favorite.flac")
    );
    assert_eq!(loaded.last_track_position_ms, 42_500);
}

#[test]
fn stream_location_round_trips_in_last_track_path() {
    let root = unique_temp_dir("state-stream");
    let original = PersistedState {
        last_track_path: Some("https://radio.example.com/live".to_string()),
        last_track_position_ms: 4_200,
        ..PersistedState::default()
    };

    original.save(&root);
    let loaded = PersistedState::load(&root);

    assert_eq!(
        loaded.last_track_path.as_deref(),
        Some("https://radio.example.com/live")
    );
    assert_eq!(loaded.last_track_position_ms, 4_200);
    assert_eq!(
        loaded.track_location(),
        Some(TrackLocation::url(
            url::Url::parse("https://radio.example.com/live").expect("valid URL")
        ))
    );
}

#[test]
fn url_looking_local_location_round_trips_in_last_track_path() {
    let root = unique_temp_dir("state-url-looking-local");
    let local = TrackLocation::local("https://radio.example.com/live");
    let original = PersistedState {
        last_track_path: Some(local.to_persisted()),
        ..PersistedState::default()
    };

    original.save(&root);
    let loaded = PersistedState::load(&root);

    assert_eq!(
        loaded.last_track_path.as_deref(),
        Some("harmonium-local-v2:68747470733a2f2f726164696f2e6578616d706c652e636f6d2f6c697665")
    );
    assert_eq!(loaded.track_location(), Some(local));
}

#[cfg(unix)]
#[test]
fn non_utf8_local_location_round_trips_in_state() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let root = unique_temp_dir("state-nonutf8-location");
    let path = std::path::PathBuf::from(OsString::from_vec(
        b"/music/"
            .iter()
            .copied()
            .chain([0xff, b'.', b'm', b'p', b'3'])
            .collect(),
    ));
    let local = TrackLocation::local(path);
    let original = PersistedState {
        last_track_path: Some(local.to_persisted()),
        ..PersistedState::default()
    };

    original.save(&root);
    let loaded = PersistedState::load(&root);

    assert_eq!(loaded.track_location(), Some(local));
}

#[test]
fn legacy_state_file_last_track_path_decodes_to_a_local_track_location() {
    let root = unique_temp_dir("state-legacy-location");
    fs::create_dir_all(&root).expect("dir");
    fs::write(
        root.join("state.toml"),
        "last_track_path = \"/music/legacy.mp3\"\nlast_track_position_ms = 900\n",
    )
    .expect("legacy state fixture");

    let loaded = PersistedState::load(&root);

    assert_eq!(
        loaded.track_location(),
        Some(TrackLocation::local("/music/legacy.mp3"))
    );
    assert_eq!(loaded.last_track_position_ms, 900);
}

#[test]
fn invalid_legacy_track_location_type_falls_back_to_state_defaults() {
    let root = unique_temp_dir("state-invalid-location");
    fs::create_dir_all(&root).expect("dir");
    fs::write(root.join("state.toml"), "last_track_path = 42\n").expect("fixture");

    let loaded = PersistedState::load(&root);

    assert_eq!(loaded, PersistedState::default());
    assert_eq!(loaded.track_location(), None);
}

#[test]
fn last_playlist_defaults_to_none_and_round_trips() {
    let state = PersistedState::default();
    assert_eq!(state.last_playlist, None);

    let root = unique_temp_dir("state-last");
    fs::create_dir_all(&root).expect("dir");
    state.save(&root);
    let loaded = PersistedState::load(&root);
    assert_eq!(loaded.last_playlist, None);

    let named = PersistedState {
        last_playlist: Some("Jazz".to_string()),
        ..PersistedState::default()
    };
    named.save(&root);
    let loaded = PersistedState::load(&root);
    assert_eq!(loaded.last_playlist.as_deref(), Some("Jazz"));
}

#[test]
fn invalid_state_file_falls_back_to_defaults() {
    let root = unique_temp_dir("state-invalid");
    fs::create_dir_all(&root).expect("dir");
    fs::write(root.join("state.toml"), "not valid toml {{{").expect("fixture");

    let state = PersistedState::load(&root);

    assert_eq!(state, PersistedState::default());
}

#[test]
fn partial_state_file_merges_with_defaults() {
    let root = unique_temp_dir("state-partial");
    fs::create_dir_all(&root).expect("dir");
    fs::write(root.join("state.toml"), "repeat_mode = \"track\"\n").expect("fixture");

    let state = PersistedState::load(&root);

    assert_eq!(state.repeat_mode, RepeatMode::Track);
    assert!(!state.shuffle);
    assert_eq!(state.last_playlist, None);
}

#[test]
fn unknown_repeat_mode_falls_back_without_discarding_other_state_fields() {
    let root = unique_temp_dir("state-unknown-repeat-mode");
    fs::create_dir_all(&root).expect("dir");
    fs::write(
        root.join(STATE_FILE_NAME),
        "repeat_mode = \"unexpected\"\nshuffle = true\n",
    )
    .expect("state fixture");

    let state = PersistedState::load(&root);

    assert_eq!(state.repeat_mode, RepeatMode::Off);
    assert!(state.shuffle);
}

#[test]
fn ensure_themes_dir_writes_bundled_themes_on_first_run() {
    let root = unique_temp_dir("themes-first-run");

    let themes_dir = ensure_themes_dir(&root);

    assert!(themes_dir.is_dir());
    assert!(themes_dir.join("default.toml").exists());
    assert!(themes_dir.join("gruvbox.toml").exists());
}

#[test]
fn bundled_themes_define_lyrics_colors_from_their_own_palette() {
    // Each starter file spells out the five lyrics keys with explicit
    // copies of that same theme's roles, so Some(...) doubles as proof
    // the key is present in the literal (unset would parse as None).
    for theme in bundled_themes() {
        let parsed: crate::ui::theme::Theme =
            crate::ui::theme::Theme::load_from_toml(theme.content)
                .unwrap_or_else(|error| panic!("{} must parse: {error}", theme.name));
        assert_eq!(
            parsed.lyrics_text,
            Some(parsed.text_muted),
            "{}: lyrics_text must equal its own text_muted",
            theme.name
        );
        assert_eq!(
            parsed.lyrics_highlight,
            Some(parsed.progress),
            "{}: lyrics_highlight must equal its own progress",
            theme.name
        );
        assert_eq!(
            parsed.lyrics_background,
            Some(parsed.background),
            "{}: lyrics_background must equal its own background",
            theme.name
        );
        assert_eq!(
            parsed.lyrics_border,
            Some(parsed.border),
            "{}: lyrics_border must equal its own border",
            theme.name
        );
        assert_eq!(
            parsed.lyrics_border_focused,
            Some(parsed.border_focused),
            "{}: lyrics_border_focused must equal its own border_focused",
            theme.name
        );
    }
}

#[test]
fn bundled_theme_color_keys_are_alphabetical() {
    // The literals are written to disk verbatim on first run, so they
    // must already match the alphabetical order write_theme_file emits.
    let mut expected: Vec<&str> = crate::ui::theme::ThemeColorField::ALL
        .into_iter()
        .map(crate::ui::theme::ThemeColorField::label)
        .collect();
    expected.sort();
    for theme in bundled_themes() {
        let keys: Vec<&str> = theme
            .content
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('['))
            .filter_map(|line| line.split_once('=').map(|(key, _)| key.trim()))
            .collect();
        assert_eq!(
            keys, expected,
            "{}: [colors] keys must be alphabetical and complete",
            theme.name
        );
    }
}

#[test]
fn ensure_themes_dir_does_not_overwrite_existing_files() {
    let root = unique_temp_dir("themes-no-overwrite");
    let themes_dir = root.join("themes");
    fs::create_dir_all(&themes_dir).expect("dir");
    fs::write(themes_dir.join("default.toml"), "custom content").expect("fixture");

    ensure_themes_dir(&root);

    let content = fs::read_to_string(themes_dir.join("default.toml")).expect("read");
    assert_eq!(content, "custom content");
}

#[test]
fn every_documented_artwork_source_value_is_accepted() {
    for (literal, expected) in [
        ("all", ArtworkSource::All),
        ("metadata", ArtworkSource::Metadata),
        ("local", ArtworkSource::Local),
        ("remote", ArtworkSource::Remote),
    ] {
        let root = unique_temp_dir("config-artwork-source");
        fs::write(
            root.join("config.toml"),
            format!("[ui]\nartwork_source = \"{literal}\"\n"),
        )
        .expect("config fixture");

        assert_eq!(AppConfig::load(&root).ui.artwork_source, expected);
    }
}

#[test]
fn unknown_artwork_source_value_falls_back_to_defaults() {
    let root = unique_temp_dir("config-unknown-source");
    fs::write(
        root.join("config.toml"),
        "[ui]\nartwork_source = \"embedded\"\n",
    )
    .expect("config fixture");

    let config = AppConfig::load(&root);
    assert_eq!(config.ui.artwork_source, ArtworkSource::default());
}

#[test]
fn general_preferences_have_defaults() {
    let general = GeneralConfig::default();
    assert!(general.confirm_quit);
    assert!(general.artwork_visible);
    assert!(!general.show_hidden, "hidden files are hidden by default");
    assert_eq!(general.volume_percent, volume(70));
}

#[test]
fn migrate_is_a_pure_typed_config_decision() {
    let legacy = LegacyPreferences {
        volume_percent: Some(60),
        confirm_quit: Some(false),
        resume_previous_track: Some(true),
        browser_directory: Some("/music".to_string()),
        artwork_visible: Some(false),
    };
    let mut config = AppConfig::default();

    assert!(migrate(&legacy, &mut config));
    assert_eq!(config.general.volume_percent, volume(60));
    assert!(!config.general.confirm_quit);
    assert!(config.general.resume_previous_track);
    assert_eq!(config.general.browser_directory, "/music");
    assert!(!config.general.artwork_visible);
}

#[test]
fn pending_legacy_preferences_respect_explicit_general_keys() {
    let root = unique_temp_dir("pending-legacy-preferences");
    fs::write(
        root.join(CONFIG_FILE_NAME),
        "[general]\nvolume_percent = 75\nconfirm_quit = true\n",
    )
    .expect("config fixture");
    let legacy = LegacyPreferences {
        volume_percent: Some(60),
        confirm_quit: Some(false),
        resume_previous_track: Some(true),
        browser_directory: Some("/music".to_string()),
        artwork_visible: Some(false),
    };

    let pending = pending_legacy_preferences(&root, &legacy);

    assert_eq!(pending.volume_percent, None);
    assert_eq!(pending.confirm_quit, None);
    assert_eq!(pending.resume_previous_track, Some(true));
    assert_eq!(pending.browser_directory.as_deref(), Some("/music"));
    assert_eq!(pending.artwork_visible, Some(false));
}

#[test]
fn erase_migrated_preferences_preserves_runtime_state_comments_and_unknown_keys() {
    let root = unique_temp_dir("state-migrate-erase");
    let original = "# Keep this state comment.\nrepeat_mode = \"track\"\nshuffle = true\nlast_playlist = \"Rock\"\nlast_track_path = \"/music/rock.mp3\"\nlast_track_position_ms = 42500\n\n# These keys are removed only after config persistence succeeds.\nvolume_percent = 60\nconfirm_quit = false\n\nunknown_state = \"keep\"\n";
    fs::write(root.join(STATE_FILE_NAME), original).expect("state fixture");

    assert!(erase_migrated_preferences(&root).expect("erase succeeds"));

    let contents = fs::read_to_string(root.join(STATE_FILE_NAME)).expect("read state");
    assert!(contents.contains("# Keep this state comment."));
    assert!(contents.contains("repeat_mode = \"track\""));
    assert!(contents.contains("shuffle = true"));
    assert!(contents.contains("last_track_position_ms = 42500"));
    assert!(contents.contains("unknown_state = \"keep\""));
    assert!(!contents.contains("volume_percent"));
    assert!(!contents.contains("confirm_quit"));

    let state = PersistedState::load(&root);
    assert_eq!(state.repeat_mode, RepeatMode::Track);
    assert!(state.shuffle);
    assert_eq!(state.last_playlist.as_deref(), Some("Rock"));
    assert_eq!(state.last_track_path.as_deref(), Some("/music/rock.mp3"));
    assert_eq!(state.last_track_position_ms, 42_500);
}

#[cfg(unix)]
#[test]
fn erase_migrated_preferences_accepts_post_rename_sync_failure_when_keys_are_gone() {
    let root = unique_temp_dir("state-migrate-post-rename-sync-failure");
    fs::write(
            root.join(STATE_FILE_NAME),
            "# Keep this runtime comment.\nrepeat_mode = \"track\"\nshuffle = true\nlast_playlist = \"Rock\"\nlast_track_path = \"/music/rock.mp3\"\nlast_track_position_ms = 42500\nvolume_percent = 60\nconfirm_quit = false\nresume_previous_track = true\nbrowser_directory = \"/music\"\nartwork_visible = false\nunknown_state = \"keep\"\n",
        )
        .expect("state fixture");

    let result = erase_migrated_preferences_with(&root, |path, contents| {
        crate::filesystem::persistence::atomic_replace_with_hooks(
            path,
            |file| std::io::Write::write_all(file, contents),
            |_| Err(std::io::Error::other("injected directory sync failure")),
        )
    });

    assert_eq!(result, Ok(true));
    let contents = fs::read_to_string(root.join(STATE_FILE_NAME)).expect("read state");
    assert!(contents.contains("# Keep this runtime comment."));
    assert!(contents.contains("unknown_state = \"keep\""));
    for key in LEGACY_PREFERENCE_KEYS {
        assert!(!contents.contains(key), "legacy key remains: {key}");
    }
}

#[test]
fn runtime_state_save_default_clears_optional_fields_and_preserves_unknown_keys() {
    let root = unique_temp_dir("state-merge-clear-optionals");
    let original = "# Keep this runtime comment.\nrepeat_mode = \"track\"\nshuffle = true\nlast_playlist = \"Rock\"\nlast_track_path = \"/music/rock.mp3\"\nlast_track_position_ms = 42500\nvolume_percent = 60\nunknown_state = \"keep\"\n";
    fs::write(root.join(STATE_FILE_NAME), original).expect("state fixture");

    PersistedState::default()
        .save_result(&root)
        .expect("state save succeeds");

    let contents = fs::read_to_string(root.join(STATE_FILE_NAME)).expect("read state");
    assert!(contents.contains("# Keep this runtime comment."));
    assert!(contents.contains("volume_percent = 60"));
    assert!(contents.contains("unknown_state = \"keep\""));
    assert!(!contents.contains("last_playlist"));
    assert!(!contents.contains("last_track_path"));
    assert_eq!(PersistedState::load(&root), PersistedState::default());
}

#[test]
fn runtime_state_save_preserves_legacy_comments_and_unknown_keys() {
    let root = unique_temp_dir("state-merge-save");
    let original = "# Keep this runtime comment.\nrepeat_mode = \"track\"\nshuffle = true\nlast_playlist = \"Rock\"\nlast_track_path = \"/music/rock.mp3\"\nlast_track_position_ms = 42500\n\n# Keep this migration value until config persistence succeeds.\nvolume_percent = 60\nunknown_state = \"keep\"\n";
    fs::write(root.join(STATE_FILE_NAME), original).expect("state fixture");

    let updated = PersistedState {
        repeat_mode: RepeatMode::All,
        shuffle: false,
        last_playlist: Some("Jazz".to_string()),
        last_track_path: Some("/music/jazz.mp3".to_string()),
        last_track_position_ms: 9000,
    };
    updated.save_result(&root).expect("state save succeeds");

    let contents = fs::read_to_string(root.join(STATE_FILE_NAME)).expect("read state");
    assert!(
        contents.contains("# Keep this runtime comment."),
        "unexpected state contents: {contents}"
    );
    assert!(contents.contains("# Keep this migration value until config persistence succeeds."));
    assert!(contents.contains("volume_percent = 60"));
    assert!(contents.contains("unknown_state = \"keep\""));
    assert!(contents.contains("repeat_mode = \"all\""));
    assert!(contents.contains("last_playlist = \"Jazz\""));
    assert!(contents.contains("last_track_path = \"/music/jazz.mp3\""));
    assert!(contents.contains("last_track_position_ms = 9000"));
}

#[test]
fn malformed_state_cleanup_and_save_preserve_source_and_bound_errors() {
    let root = unique_temp_dir("state-malformed-preserve");
    let path = root.join(STATE_FILE_NAME);
    let malformed = "volume_percent = \"migration-secret\n";
    fs::write(&path, malformed).expect("state fixture");

    let cleanup_error = erase_migrated_preferences(&root).expect_err("cleanup must fail");
    assert_eq!(
        cleanup_error,
        format!(
            "cannot parse state {} while removing migrated preferences",
            path.display()
        )
    );
    assert!(!cleanup_error.contains("migration-secret"));
    assert_eq!(fs::read_to_string(&path).expect("read state"), malformed);

    let save_error = PersistedState::default()
        .save_result(&root)
        .expect_err("save must fail");
    assert_eq!(
        save_error.to_string(),
        format!(
            "invalid state {}; existing file was left unchanged",
            path.display()
        )
    );
    assert!(!save_error.to_string().contains("migration-secret"));
    assert_eq!(fs::read_to_string(&path).expect("read state"), malformed);
}

#[test]
fn artwork_source_defaults_to_all_in_ui_config() {
    let config = UiConfig::default();
    assert_eq!(config.artwork_source, ArtworkSource::All);
}

#[test]
fn merge_aware_save_preserves_comments_and_unknown_sections() {
    let root = unique_temp_dir("config-merge");
    let initial = "# user comment\n[ui]\nshow_album_art = false\n\n[unknown]\nfoo = 1\n";
    fs::write(root.join("config.toml"), initial).expect("fixture");
    let mut cfg = AppConfig::load(&root);
    cfg.keys.quit = key("Q");
    cfg.sound.output_sink_id = "alsa_output.pci-0000_13_00.6.analog-stereo".to_string();
    cfg.save(&root).expect("save config");
    let contents = fs::read_to_string(root.join("config.toml")).expect("read");
    assert!(contents.contains("# user comment"), "comment preserved");
    assert!(contents.contains("[unknown]"), "unknown section preserved");
    assert!(contents.contains("foo = 1"), "unknown value preserved");
}

#[test]
fn save_merges_owned_sections_without_losing_nested_unknown_content() {
    let root = unique_temp_dir("config-nested-merge");
    let initial = "# Keep the file layout.\n\n[ui]\n# Keep this owned-key comment.\nshow_album_art = false\n# Keep this UI extension.\nui_extension = \"ui-secret\"\n\n[general]\nconfirm_quit = true\ngeneral_extension = 7\n\n[general.playlist_columns]\n# Keep this nested extension.\ndisplay_by = \"metadata\"\nnested_extension = \"nested-secret\"\n\n[keys]\nquit = \"q\"\nkeys_extension = true\n\n[playback]\nremote_lyrics = false\nplayback_extension = 1.5\n\n[sound]\noutput_sink_id = \"old\"\nsound_extension = \"preserve\"\n\n[log]\nlevel = \"info\"\nlog_extension = \"keep\"\n";
    fs::write(root.join("config.toml"), initial).expect("fixture");

    let mut config = AppConfig::load(&root);
    config.ui.show_album_art = true;
    config.general.confirm_quit = false;
    config.keys.quit = key("Q");
    config.playback.remote_lyrics = true;
    config.sound.output_sink_id = "new".to_string();
    config.log.level = "debug".to_string();
    config.save(&root).expect("save config");

    let contents = fs::read_to_string(root.join("config.toml")).expect("read");
    for expected in [
        "# Keep the file layout.",
        "# Keep this owned-key comment.",
        "# Keep this UI extension.",
        "ui_extension = \"ui-secret\"",
        "general_extension = 7",
        "# Keep this nested extension.",
        "nested_extension = \"nested-secret\"",
        "keys_extension = true",
        "playback_extension = 1.5",
        "sound_extension = \"preserve\"",
        "log_extension = \"keep\"",
    ] {
        assert!(
            contents.contains(expected),
            "missing preserved content: {expected}"
        );
    }
    assert!(contents.contains("show_album_art = true"));
    assert!(contents.contains("confirm_quit = false"));
    assert!(contents.contains("quit = \"Q\""));
    assert!(contents.contains("remote_lyrics = true"));
    assert!(contents.contains("output_sink_id = \"new\""));
    assert!(contents.contains("level = \"debug\""));
}

#[test]
fn structured_save_preserves_comments_attached_to_unknown_sections() {
    let root = unique_temp_dir("config-unknown-comment");
    fs::write(
            root.join("config.toml"),
            "[ui]\nshow_album_art = false\n\n# Keep this section and its comment.\n[unknown]\nvalue = 7\n",
        )
        .expect("fixture");

    AppConfig::default().save(&root).expect("save config");

    let contents = fs::read_to_string(root.join("config.toml")).expect("read");
    assert!(contents.contains("# Keep this section and its comment.\n[unknown]"));
    assert!(contents.contains("value = 7"));
}

#[test]
fn structured_save_preserves_multiline_unknown_strings_with_table_like_lines() {
    let root = unique_temp_dir("config-multiline");
    let initial = "[unknown]\ndescription = \"\"\"first line\n[x]\nlast line\n\"\"\"\n";
    fs::write(root.join("config.toml"), initial).expect("fixture");

    AppConfig::default().save(&root).expect("save config");

    let contents = fs::read_to_string(root.join("config.toml")).expect("read");
    assert!(contents.contains("description = \"\"\"first line\n[x]\nlast line\n\"\"\""));
    assert_eq!(contents.matches("[x]").count(), 1);
}

#[test]
fn structured_save_replaces_nested_owned_tables_before_their_parent() {
    let root = unique_temp_dir("config-nested-before-parent");
    fs::write(
            root.join("config.toml"),
            "[general.playlist_columns]\ndisplay_by = \"metadata\"\nmetadata_artist = true\n\n[general]\nconfirm_quit = false\n\n[unknown]\nvalue = 9\n",
        )
        .expect("fixture");

    let mut config = AppConfig::default();
    config.general.confirm_quit = true;
    config.general.playlist_columns.display_by = SortBy::Filename;
    config.save(&root).expect("save config");

    let contents = fs::read_to_string(root.join("config.toml")).expect("read");
    assert_eq!(contents.matches("[general]").count(), 1);
    assert_eq!(contents.matches("[general.playlist_columns]").count(), 1);
    assert!(contents.contains("display_by = \"filename\""));
    assert!(contents.contains("[unknown]\nvalue = 9"));
    assert_eq!(AppConfig::load(&root), config);
}

#[test]
fn malformed_config_is_not_destructively_replaced_by_save() {
    let root = unique_temp_dir("config-malformed-save");
    let malformed = "[ui]\nprivate_token = \"super-secret\n";
    fs::write(root.join("config.toml"), malformed).expect("fixture");

    let mut config = AppConfig::default();
    config.ui.show_album_art = false;
    let error = config
        .save(&root)
        .expect_err("malformed config must reject save");

    assert_eq!(
        fs::read_to_string(root.join("config.toml")).expect("read"),
        malformed
    );
    assert!(matches!(error, ConfigSaveError::Parse { .. }));
    assert!(
        error
            .to_string()
            .contains(&root.join("config.toml").display().to_string())
    );
    assert!(!error.to_string().contains("super-secret"));
}

#[test]
fn all_owned_sections_round_trip_through_structured_save() {
    let root = unique_temp_dir("config-all-owned");
    let mut config = AppConfig::default();
    config.ui.show_album_art = false;
    config.ui.album_art_mode = AlbumArtMode::Unicode;
    config.ui.theme = "night theme".to_string();
    config.ui.artwork_source = ArtworkSource::Remote;
    config.ui.border_type = BorderType::Double;
    config.general.confirm_quit = false;
    config.general.volume_percent = volume(42);
    config.general.resume_previous_track = true;
    config.general.browser_directory = "/music/\"live\"".to_string();
    config.general.artwork_visible = false;
    config.general.show_hidden = true;
    config.general.playlist_columns.display_by = SortBy::Metadata;
    config.general.playlist_columns.metadata_artist = true;
    config.general.playlist_columns.metadata_album = true;
    config.general.playlist_columns.metadata_track_number = true;
    config.general.now_playing_display.sort_by = SortBy::Metadata;
    config.general.now_playing_display.metadata_title = true;
    config.keys.quit = key("Q");
    config.keys.lyrics = key("L");
    config.playback.remote_lyrics = true;
    config.playback.gain_db = gain(4.5);
    config.playback.crossfade_seconds = crossfade(15);
    config.sound.output_sink_id = "alsa_output.test".to_string();
    config.log.level = "debug".to_string();

    config.save(&root).expect("save config");

    assert_eq!(AppConfig::load(&root), config);
}

#[test]
fn playlist_columns_round_trip_and_legacy_sort_tracks_migrate() {
    let root = unique_temp_dir("config-playlist-columns");
    let mut cfg = AppConfig::default();
    cfg.general.playlist_columns.display_by = SortBy::Metadata;
    cfg.general.playlist_columns.metadata_artist = true;
    cfg.general.playlist_columns.metadata_track_number = true;
    cfg.save(&root).expect("save config");
    let contents = fs::read_to_string(root.join("config.toml")).expect("read");
    assert_eq!(
        contents.matches("[general.playlist_columns]").count(),
        1,
        "must contain exactly one nested playlist_columns table: {contents}"
    );
    assert!(
        !contents.contains("sort_tracks"),
        "obsolete sort_tracks must not be written: {contents}"
    );
    assert!(contents.contains("display_by = \"metadata\""));
    assert!(contents.contains("metadata_artist = true"));
    assert!(contents.contains("metadata_track_number = true"));

    let loaded = AppConfig::load(&root);
    assert_eq!(
        loaded.general.playlist_columns,
        cfg.general.playlist_columns
    );

    fs::write(
            root.join("config.toml"),
            "[general.sort_tracks]\nsort_by = \"metadata\"\nmetadata_artist = true\nmetadata_album = true\nmetadata_track_number = true\nmetadata_title = false\n",
        )
        .expect("legacy fixture");
    let migrated = AppConfig::load(&root);
    assert_eq!(
        migrated.general.playlist_columns.display_by,
        SortBy::Metadata
    );
    assert!(migrated.general.playlist_columns.metadata_artist);
    assert!(migrated.general.playlist_columns.metadata_album);
    assert!(migrated.general.playlist_columns.metadata_track_number);
    migrated.save(&root).expect("save config");
    let migrated_contents = fs::read_to_string(root.join("config.toml")).expect("read");
    assert!(
        !migrated_contents.contains("sort_tracks"),
        "obsolete setting survived migration: {migrated_contents}"
    );
}

#[test]
fn sound_config_round_trips() {
    let root = unique_temp_dir("config-sound");
    let cfg = AppConfig {
        sound: SoundConfig {
            output_sink_id: "alsa_output.pci-0000_13_00.6.analog-stereo".to_string(),
        },
        ..Default::default()
    };
    fs::write(root.join("config.toml"), toml::to_string(&cfg).unwrap()).expect("write");
    let loaded = AppConfig::load(&root);
    assert_eq!(
        loaded.sound.output_sink_id,
        "alsa_output.pci-0000_13_00.6.analog-stereo"
    );
}

#[test]
fn sound_config_defaults_to_session_default() {
    let cfg = SoundConfig::default();
    assert_eq!(cfg.output_sink_id, "");
}

#[test]
fn playback_config_round_trips() {
    let root = unique_temp_dir("config-playback");
    fs::write(
        root.join("config.toml"),
        "[playback]\nremote_lyrics = true\n",
    )
    .expect("config fixture");

    let config = AppConfig::load(&root);

    assert!(
        config.playback.remote_lyrics,
        "the remote lyrics toggle must round trip through the file"
    );
}

#[test]
fn playback_config_defaults_to_remote_lyrics_off() {
    assert!(!PlaybackConfig::default().remote_lyrics);
    assert!(
        !AppConfig::default().playback.remote_lyrics,
        "a fresh install keeps the local-only lyrics behavior by default"
    );
}

#[test]
fn save_writes_and_reloads_the_playback_section() {
    // The owned-section rewrite must carry `[playback]`, otherwise a
    // toggle would silently revert on the next launch.
    let root = unique_temp_dir("config-playback-save");
    let mut cfg = AppConfig::default();
    cfg.playback.remote_lyrics = true;
    cfg.save(&root).expect("save config");

    let contents = fs::read_to_string(root.join("config.toml")).expect("read");
    assert!(
        contents.contains("[playback]"),
        "must emit [playback]: {contents}"
    );
    assert!(
        contents.contains("remote_lyrics = true"),
        "must persist the toggle: {contents}"
    );

    assert!(AppConfig::load(&root).playback.remote_lyrics);
}

#[test]
fn playback_gain_round_trips_through_save_and_reload() {
    let root = unique_temp_dir("config-playback-gain");
    let mut cfg = AppConfig::default();
    cfg.playback.gain_db = gain(6.0);
    cfg.save(&root).expect("save config");

    let contents = fs::read_to_string(root.join("config.toml")).expect("read");
    assert!(
        contents.contains("gain_db = 6.0"),
        "must persist the gain in dB: {contents}"
    );

    let reloaded = AppConfig::load(&root);
    assert_eq!(reloaded.playback.gain_db, gain(6.0));
}

#[test]
fn crossfade_defaults_to_disabled_and_only_allows_multiple_of_5() {
    // Disabled by default (0 s).
    assert_eq!(
        AppConfig::default().playback.crossfade_seconds,
        CrossfadeSeconds::DISABLED
    );
    assert_eq!(
        PlaybackConfig::default().crossfade_seconds,
        CrossfadeSeconds::DISABLED
    );
    // The enabled domain is 5..=30 in steps of 5.
    assert_eq!(CROSSFADE_MIN_SECONDS, 5);
    assert_eq!(CROSSFADE_MAX_SECONDS, 30);
    assert_eq!(CROSSFADE_STEP_SECONDS, 5);
    for n in
        (CROSSFADE_MIN_SECONDS..=CROSSFADE_MAX_SECONDS).step_by(CROSSFADE_STEP_SECONDS as usize)
    {
        assert_eq!(n % CROSSFADE_STEP_SECONDS, 0, "{n} is a valid step");
    }
    // A non-multiple value is outside the allowed domain.
    assert_eq!(12 % CROSSFADE_STEP_SECONDS, 2);
}

#[test]
fn crossfade_round_trips_through_save_and_reload() {
    let root = unique_temp_dir("config-playback-crossfade");
    let mut cfg = AppConfig::default();
    cfg.playback.crossfade_seconds = crossfade(15);
    cfg.save(&root).expect("save config");

    let contents = fs::read_to_string(root.join("config.toml")).expect("read");
    assert!(
        contents.contains("crossfade_seconds = 15"),
        "must persist the crossfade in seconds: {contents}"
    );

    let reloaded = AppConfig::load(&root);
    assert_eq!(reloaded.playback.crossfade_seconds, crossfade(15));
}

#[test]
fn load_clamps_volume_percent_out_of_range() {
    let root = unique_temp_dir("config-sanitize-volume");
    fs::write(
        root.join("config.toml"),
        "[general]\nvolume_percent = 250\nconfirm_quit = true\n",
    )
    .expect("write");

    let cfg = AppConfig::load(&root);
    assert_eq!(cfg.general.volume_percent, volume(100));
}

#[test]
fn load_clamps_gain_db_and_keeps_a_negative_slider_value() {
    let root = unique_temp_dir("config-sanitize-gain");
    fs::write(root.join("config.toml"), "[playback]\ngain_db = 99.0\n").expect("write");

    // +99 dB is a ~89.000x amplification factor; the config must clamp it
    // back to the documented ceiling rather than hand it to the engine.
    let cfg = AppConfig::load(&root);
    assert_eq!(cfg.playback.gain_db, GainDb::MAX);

    // The legal lower half of the range stays untouched.
    fs::write(root.join("config.toml"), "[playback]\ngain_db = -6.0\n").expect("write");
    let cfg = AppConfig::load(&root);
    assert_eq!(cfg.playback.gain_db, gain(-6.0));
}

#[test]
fn load_resets_non_finite_gain_db_to_zero() {
    let root = unique_temp_dir("config-sanitize-gain-nan");
    fs::write(root.join("config.toml"), "[playback]\ngain_db = NaN\n").expect("write");

    let cfg = AppConfig::load(&root);
    assert_eq!(cfg.playback.gain_db, GainDb::default());
}

#[test]
fn load_clamps_and_snaps_crossfade_into_the_documented_domain() {
    let root = unique_temp_dir("config-sanitize-crossfade");

    // Out of the enabled ceiling: clamp to 30.
    fs::write(
        root.join("config.toml"),
        "[playback]\ncrossfade_seconds = 47\n",
    )
    .expect("write");
    let cfg = AppConfig::load(&root);
    assert!(cfg.playback.crossfade_seconds <= CrossfadeSeconds::MAX);
    // 47 is past the ceiling, so it is clamped to 30 (already a step).
    assert_eq!(cfg.playback.crossfade_seconds, CrossfadeSeconds::MAX);

    // A stray non-multiple within range snaps to its nearest step.
    fs::write(
        root.join("config.toml"),
        "[playback]\ncrossfade_seconds = 12\n",
    )
    .expect("write");
    let cfg = AppConfig::load(&root);
    assert_eq!(cfg.playback.crossfade_seconds, crossfade(10));

    // Zero (disabled) is an accepted domain value, never snapped.
    fs::write(
        root.join("config.toml"),
        "[playback]\ncrossfade_seconds = 0\n",
    )
    .expect("write");
    let cfg = AppConfig::load(&root);
    assert_eq!(cfg.playback.crossfade_seconds, CrossfadeSeconds::DISABLED);
}

#[test]
fn crossfade_boundary_normalization_is_idempotent() {
    for (input, expected) in [
        (0, 0),
        (1, 0),
        (4, 0),
        (5, 5),
        (7, 5),
        (8, 5),
        (30, 30),
        (31, 30),
    ] {
        let normalized = CrossfadeSeconds::from_boundary(input);
        assert_eq!(normalized, crossfade(expected), "input: {input}");
        assert_eq!(
            CrossfadeSeconds::from_boundary(normalized.as_u16()),
            normalized
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 48,
        failure_persistence: None,
        max_shrink_iters: 128,
        rng_algorithm: proptest::test_runner::RngAlgorithm::ChaCha,
        rng_seed: proptest::test_runner::RngSeed::Fixed(0x4933_3002),
        .. ProptestConfig::default()
    })]

    #[test]
    fn boundary_normalization_keeps_arbitrary_numeric_values_in_legal_domains(
        volume in any::<u16>(),
        gain in any::<f32>(),
        crossfade in any::<u16>(),
    ) {
        let mut config = AppConfig::default();
        config.general.volume_percent = VolumePercent::from_boundary(volume);
        config.playback.gain_db = GainDb::from_boundary(gain).unwrap_or_default();
        config.playback.crossfade_seconds = CrossfadeSeconds::from_boundary(crossfade);

        prop_assert!(config.general.volume_percent.as_u16() <= 100);
        prop_assert!((GainDb::MIN..=GainDb::MAX).contains(&config.playback.gain_db));
        prop_assert!(config.playback.crossfade_seconds == CrossfadeSeconds::DISABLED
            || (CrossfadeSeconds::MIN..=CrossfadeSeconds::MAX)
                .contains(&config.playback.crossfade_seconds)
                && config.playback.crossfade_seconds.as_u8() % CrossfadeSeconds::STEP == 0);
    }
}
