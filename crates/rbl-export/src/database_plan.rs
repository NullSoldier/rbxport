//! Author both device databases from declared media paths and metadata.
//! Audio publication is the caller's responsibility.

use super::{
    album_row, analysis_directory, analysis_slot_key, artwork_id_from_path, artwork_path,
    artwork_row, artist_row, assign_ids, build_pdb, ext_pdb, export_root_name,
    key_row, layouts, one_library_error, path_key, playlist_entry_row,
    playlist_ids, playlist_row, reconcile, sibling_positions, simple_named_row,
    snapshot, track_database_rows, write_one_library, BTreeMap, BTreeSet,
    ExportError, Intern, Interns, Layout, Manifest, Path, PathBuf, PdbTables,
    Result, SourcePlaylist, SourceTrack, TrackDatabaseFields, ANLZ_NAME, PAGE_SIZE,
};

/// One database generation and its exact media destinations. Paths are
/// device-relative and must be published with the databases as one operation.
pub struct ExportPlan {
    pub tracks: Vec<PlannedMedia>,
    pub playlists: Vec<PlannedPlaylist>,
    pub removed: Vec<RemovedMedia>,
    pub export_pdb: Vec<u8>,
    pub export_ext_pdb: Vec<u8>,
    pub export_library_db: Vec<u8>,
}

pub struct PlannedMedia {
    pub library_id: u64,
    pub device_id: u32,
    pub path: String,
    pub analysis_dir: String,
    pub preserved: bool,
}

pub struct PlannedPlaylist {
    pub library_id: u64,
    pub device_id: u32,
    pub device_only: bool,
}

pub struct RemovedMedia {
    pub device_id: u32,
    pub path: String,
    pub analysis_dir: String,
}

fn remove_unreferenced_owned(
    tracks: &mut Vec<SourceTrack>,
    playlists: &mut [SourcePlaylist],
    selected_count: usize,
    history: &[snapshot::History],
    owned: &BTreeSet<u32>,
) -> Result<Vec<RemovedMedia>> {
    let referenced: BTreeSet<usize> = playlists.iter().flat_map(|p| p.track_indices.iter().copied()).collect();
    let history_ids: BTreeSet<u32> = history.iter().flat_map(|h| h.tracks.iter().copied()).collect();
    let mut remap = vec![None; tracks.len()];
    let mut removed = Vec::new();
    let mut kept = Vec::with_capacity(tracks.len());
    for (index, track) in tracks.drain(..).enumerate() {
        let remove = track.device.as_ref().is_some_and(|device|
            owned.contains(&device.id) && index >= selected_count && !referenced.contains(&index)
                && !history_ids.contains(&device.id));
        if remove {
            if let Some(device) = track.device {
                removed.push(RemovedMedia { device_id: device.id, path: device.audio, analysis_dir: device.analysis_dir });
            }
        } else {
            remap[index] = Some(kept.len());
            kept.push(track);
        }
    }
    *tracks = kept;
    for playlist in playlists {
        for index in &mut playlist.track_indices {
            *index = remap.get(*index).and_then(|mapped| *mapped)
                .ok_or_else(|| ExportError::Conflict("An owned track is still referenced".into()))?;
        }
    }
    Ok(removed)
}

fn validate_planned_paths(selected: &[SourceTrack]) -> Result<()> {
    use unicode_normalization::UnicodeNormalization;
    for track in selected {
        let Some(audio) = track.planned_audio_path.as_deref() else {
            return Err(ExportError::Conflict("A planned track has no media destination".into()));
        };
        let segments: Vec<_> = audio.split('/').collect();
        if segments.len() < 4 || !segments[0].is_empty() || segments[1] != "Contents"
            || segments[2..].iter().any(|part| part.is_empty() || *part == "." || *part == "..")
            || audio.contains('\\') || audio.nfc().ne(audio.chars()) || track.file_size == 0 {
            return Err(ExportError::Conflict(format!("Invalid planned media path or size: {audio}")));
        }
    }
    Ok(())
}

fn planned_layouts(tracks: &[SourceTrack], ids: &[u32], root_name: &str, previous: Option<&Manifest>) -> Result<Vec<Layout>> {
    let in_place = vec![None; tracks.len()];
    let mut places = layouts(tracks, ids, root_name, previous, &in_place, &BTreeSet::new());
    let mut used_audio: BTreeSet<_> = places.iter().map(|place| path_key(&place.audio)).collect();
    let mut used_analysis: BTreeSet<_> = tracks.iter().zip(&places)
        .filter(|(track, _)| track.device.as_ref().is_some_and(|device| device.preserve))
        .map(|(_, place)| analysis_slot_key(&place.anlz_dir, &place.anlz_name))
        .collect();
    for ((track, id), place) in tracks.iter().zip(ids).zip(&mut places) {
        if track.device.as_ref().is_some_and(|device| device.preserve) || !track.planned_analysis {
            continue;
        }
        let original = PathBuf::from(&place.audio);
        let stem = original.file_stem().unwrap_or_default().to_string_lossy();
        let extension = original.extension().map_or(String::new(), |value| format!(".{}", value.to_string_lossy()));
        let parent = original.parent().unwrap_or(Path::new("/Contents"));
        let mut reserved = false;
        for suffix in 0..=200_003_u32 {
            let candidate = if suffix == 0 { place.audio.clone() }
                else { format!("{}/{}-{id}-{suffix}{extension}", parent.display(), stem) };
            if suffix != 0 && used_audio.contains(&path_key(&candidate)) { continue; }
            let directory = analysis_directory(&candidate, root_name);
            if !used_analysis.insert(analysis_slot_key(&directory, ANLZ_NAME)) { continue; }
            if suffix != 0 {
                used_audio.insert(path_key(&candidate));
                place.audio = candidate;
                place.file_name = Path::new(&place.audio).file_name().unwrap_or_default().to_string_lossy().into_owned();
            }
            place.anlz_dir = directory;
            reserved = true;
            break;
        }
        if !reserved {
            return Err(ExportError::Conflict("Analysis directory space exhausted".into()));
        }
    }
    Ok(places)
}

/// Build a complete database generation without opening, copying, converting,
/// or analyzing the selected audio. Each selected track supplies a declared
/// `/Contents/...` destination and its known byte size. Existing device-only
/// tracks and playlists are reconciled from the baseline databases.
pub fn export_plan(destination: &Path, selected: &[SourceTrack], selected_playlists: &[SourcePlaylist], owned_device_ids: &BTreeSet<u32>) -> Result<ExportPlan> {
    validate_planned_paths(selected)?;
    if selected.iter().any(|track| track.id > i64::MAX as u64) {
        return Err(ExportError::Conflict("A library track ID exceeds OneLibrary's signed integer range".into()));
    }
    let root_name = export_root_name(destination)?;
    let before = snapshot::Snapshot::read_at(destination, root_name)?;
    let mut used_playlist_ids = BTreeSet::new();
    for playlist in selected_playlists.iter().filter(|playlist| playlist.device_id != 0) {
        if !used_playlist_ids.insert(playlist.device_id) {
            return Err(ExportError::Conflict(format!("Device playlist {} matched more than once", playlist.device_id)));
        }
        if !before.legacy.iter().chain(&before.one).any(|library| library.playlists.iter()
            .any(|node| node.id == playlist.device_id && node.folder == playlist.folder)) {
            return Err(ExportError::Conflict(format!("Matched device playlist {} is missing or has a different type", playlist.device_id)));
        }
    }
    let previous = Manifest::load(destination);
    let existing = reconcile::capture_sources(destination, &before)?;
    let mut matched = selected.to_vec();
    let mut used_existing = BTreeSet::new();
    for track in &mut matched {
        // A track this planner wrote previously carries its full library id
        // in Device Library Plus. Prefer that identity over a caller's
        // first-adoption metadata match, which can become stale after edits.
        let stable = before.identity.iter().find_map(|(id, (db, content))|
            (*db == 0 && *content == track.id && existing.get(id).is_some_and(|source| source.file_size == track.file_size))
                .then_some(*id));
        let Some(id) = stable.or(track.existing_device_id) else { continue };
        if !used_existing.insert(id) {
            return Err(ExportError::Conflict(format!("Device track {id} matched more than once")));
        }
        let source = existing.get(&id).ok_or_else(|| ExportError::Conflict(format!("Matched device track {id} is missing")))?;
        let mut device = source.device.clone().ok_or_else(|| ExportError::Conflict(format!("Matched device track {id} has no device metadata")))?;
        if track.file_size != source.file_size ||
            (stable.is_none() && !track.title.trim().eq_ignore_ascii_case(source.title.trim())) {
            return Err(ExportError::Conflict(format!("Matched device track {id} does not match the selected track")));
        }
        device.preserve = true;
        device.master_db_id = 0;
        device.master_content_id = track.id;
        track.device = Some(device);
    }
    let reconciled = reconcile::prepare(destination, &before, previous.as_ref(), &matched, selected_playlists, 0, true, None)?;
    let mut tracks = reconciled.tracks;
    let mut playlists = reconciled.playlists;
    let removed = remove_unreferenced_owned(&mut tracks, &mut playlists, selected.len(), &before.history, owned_device_ids)?;
    let ids = assign_ids(&tracks, previous.as_ref());
    if ids.iter().collect::<BTreeSet<_>>().len() != ids.len() {
        return Err(ExportError::Conflict("Duplicate device track identity".into()));
    }
    let playlist_ids = playlist_ids(&playlists, previous.as_ref())?;
    let layouts = planned_layouts(&tracks, &ids, root_name, previous.as_ref())?;
    let mut artists = Intern::default();
    let mut albums = Intern::default();
    let mut genres = Intern::default();
    let mut labels = Intern::default();
    let mut keys = Intern::default();
    let mut interns = Interns {
        artists: &mut artists, albums: &mut albums, genres: &mut genres,
        labels: &mut labels, keys: &mut keys,
    };
    let mut track_rows = Vec::with_capacity(tracks.len());
    let mut one_tracks = Vec::with_capacity(tracks.len());
    let mut media = Vec::with_capacity(tracks.len());
    let mut artwork_paths = BTreeMap::<u32, String>::new();
    for ((track, id), place) in tracks.iter().zip(&ids).zip(&layouts) {
        let preserved = track.device.as_ref().is_some_and(|d| d.preserve);
        let audio = if preserved { track.device.as_ref().map_or(&place.audio, |d| &d.audio) } else { &place.audio };
        let analysis_dir = if preserved || track.planned_analysis { place.anlz_dir.as_str() } else { "" };
        let analysis_path = if analysis_dir.is_empty() { String::new() } else { format!("{analysis_dir}/{}.DAT", place.anlz_name) };
        let size = if track.file_size > 0 { track.file_size } else { std::fs::metadata(&track.source_path).map_or(0, |m| m.len()) };
        let master_db_id = track.device.as_ref().map_or(0, |d| d.master_db_id);
        let master_content_id = track.device.as_ref().map_or(track.id, |d| d.master_content_id);
        let filename = Path::new(audio).file_name().unwrap_or_default().to_string_lossy().into_owned();
        let artwork_id = if preserved { track.artwork.as_deref().map_or(0, |p| artwork_id_from_path(&p.to_string_lossy())) } else { 0 };
        if artwork_id != 0 {
            artwork_paths.insert(artwork_id, artwork_path(artwork_id, "a", false).replacen("/PIONEER/", &format!("/{root_name}/"), 1));
        }
        media.push(PlannedMedia { library_id: track.id, device_id: *id, path: audio.clone(), analysis_dir: analysis_dir.into(), preserved });
        let (one, legacy) = track_database_rows(track, TrackDatabaseFields {
            export_id: *id, library_id: master_content_id, master_db_id,
            file_size: size, legacy_file_size: size, artwork_id,
            one_bitrate: track.bitrate, legacy_bitrate: track.bitrate,
            sample_rate: track.sample_rate, metadata: track.metadata.clone(),
            audio_path: audio.clone(), file_name: filename, analysis_path,
        }, &mut interns);
        one_tracks.push(one);
        track_rows.push(legacy);
    }
    let positions = sibling_positions(&playlists);
    let mut playlist_rows = Vec::new();
    let mut entry_rows = Vec::new();
    for (index, playlist) in playlists.iter().enumerate() {
        let id = playlist_ids[index];
        let parent = playlists.iter().position(|p| p.id != 0 && p.id == playlist.parent_id).map_or(0, |p| playlist_ids[p]);
        playlist_rows.push(playlist_row(id, parent, positions[index], playlist.folder, &playlist.name));
        for (position, track_index) in playlist.track_indices.iter().enumerate() {
            let device_id = *ids.get(*track_index).ok_or_else(|| ExportError::Conflict("Invalid planned playlist membership".into()))?;
            entry_rows.push(playlist_entry_row(u32::try_from(position + 1).unwrap_or(u32::MAX), device_id, id));
        }
    }
    let settings_path = destination.join(root_name).join("rekordbox/exportLibrary.db");
    let settings = if settings_path.exists() {
        Some(rbl_onelibrary::settings::StickSettings::read(&settings_path).map_err(|e| one_library_error(&e))?)
    } else { None };
    let genre_rows: Vec<_> = genres.entries().map(|(id, name)| simple_named_row(id, name)).collect();
    let artist_rows: Vec<_> = artists.entries().map(|(id, name)| artist_row(id, name)).collect();
    let album_rows: Vec<_> = albums.entries().map(|(id, name)| album_row(id, 0, name)).collect();
    let label_rows: Vec<_> = labels.entries().map(|(id, name)| simple_named_row(id, name)).collect();
    let key_rows: Vec<_> = keys.entries().map(|(id, name)| key_row(id, name)).collect();
    let limit = rbl_pdb::build::max_row_len(PAGE_SIZE);
    if track_rows.iter().chain(&playlist_rows).chain(&genre_rows).chain(&artist_rows)
        .chain(&album_rows).chain(&label_rows).chain(&key_rows).any(|row| row.len() > limit) {
        return Err(ExportError::Conflict("A planned database row exceeds the DeviceSQL page limit".into()));
    }
    let artwork_paths: Vec<_> = artwork_paths.into_iter().collect();
    let artwork_rows: Vec<_> = artwork_paths.iter().map(|(id, path)| artwork_row(*id, path)).collect();
    let export_pdb = build_pdb(&PdbTables {
        tracks: &track_rows, genres: &genre_rows, artists: &artist_rows, albums: &album_rows,
        labels: &label_rows, keys: &key_rows, playlists: &playlist_rows, entries: &entry_rows,
        artwork: &artwork_rows, history: &before.history, colors: settings.as_ref().map_or(&[], |s| s.colors.as_slice()),
        device_name: settings.as_ref().map_or("", |s| s.device_name.as_str()),
        background: before.legacy_background, menus: settings.as_ref(),
    });
    let tagged: Vec<_> = one_tracks.iter().map(|t| (t.export_id, t.my_tags.as_slice())).collect();
    let export_ext_pdb = ext_pdb::build(&before.my_tags, &tagged, 0);
    let temporary = tempfile::tempdir()?;
    write_one_library(temporary.path(), &one_tracks, &playlists, &playlist_ids,
        &ids.iter().copied().map(Some).collect::<Vec<_>>(), &artwork_paths, &before.my_tags,
        settings.as_ref(), 0, &before, settings_path.exists().then_some(settings_path.as_path()))?;
    let export_library_db = std::fs::read(temporary.path().join("exportLibrary.db"))?;
    Ok(ExportPlan {
        tracks: media,
        playlists: playlists.iter().zip(playlist_ids).map(|(p, id)| PlannedPlaylist { library_id: p.id, device_id: id, device_only: p.device_only }).collect(),
        removed,
        export_pdb, export_ext_pdb, export_library_db,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn authors_both_databases_without_audio() {
        let root = tempfile::tempdir().unwrap();
        let planned = SourceTrack {
            id: 91, source_path: root.path().join("absent.mp3"),
            planned_audio_path: Some("/Contents/Artist/Album/absent.mp3".into()),
            planned_analysis: true,
            title: "Planned track".into(), artist: "Artist".into(), album: "Album".into(),
            file_size: 2048, ..Default::default()
        };
        let list = SourcePlaylist { id: 12, name: "Set".into(), track_indices: vec![0], ..Default::default() };
        let result = export_plan(root.path(), &[planned], &[list], &BTreeSet::new()).unwrap();
        assert!(!root.path().join("absent.mp3").exists());
        assert_eq!(result.tracks[0].path, "/Contents/Artist/Album/absent.mp3");
        assert!(result.tracks[0].analysis_dir.starts_with("/PIONEER/USBANLZ/"));
        let db = root.path().join("PIONEER/rekordbox");
        std::fs::create_dir_all(&db).unwrap();
        std::fs::write(db.join("export.pdb"), &result.export_pdb).unwrap();
        std::fs::write(db.join("exportExt.pdb"), &result.export_ext_pdb).unwrap();
        std::fs::write(db.join("exportLibrary.db"), &result.export_library_db).unwrap();
        assert!(crate::verify_databases(root.path()).unwrap().is_ok());
        let library = crate::device_library::read(root.path(), crate::device_library::Format::OneLibrary).unwrap();
        assert_eq!(library.tracks[0].path, result.tracks[0].path);
        assert!(library.tracks[0].analysis_path.starts_with(&result.tracks[0].analysis_dir));
    }

    #[test]
    fn carries_device_only_tracks_and_playlists_from_database_baseline() {
        let original = tempfile::tempdir().unwrap();
        let source = original.path().join("owned.mp3");
        std::fs::write(&source, b"owned audio").unwrap();
        let device = original.path().join("device");
        crate::export(&device, &[SourceTrack {
            id: 17, source_path: source, title: "USB owned".into(), file_size: 11,
            ..Default::default()
        }], &[SourcePlaylist { id: 17, name: "USB set".into(), track_indices: vec![0], ..Default::default() }]).unwrap();
        let server = tempfile::tempdir().unwrap();
        let db = server.path().join("PIONEER/rekordbox");
        std::fs::create_dir_all(&db).unwrap();
        for name in ["export.pdb", "exportExt.pdb", "exportLibrary.db"] {
            std::fs::copy(device.join("PIONEER/rekordbox").join(name), db.join(name)).unwrap();
        }
        let result = export_plan(server.path(), &[SourceTrack {
            id: 91, source_path: server.path().join("absent.mp3"),
            planned_audio_path: Some("/Contents/Artist/absent.mp3".into()),
            title: "New".into(), file_size: 2048, ..Default::default()
        }], &[SourcePlaylist { id: 91, name: "New set".into(), track_indices: vec![0], ..Default::default() }], &BTreeSet::new()).unwrap();
        assert_eq!(result.tracks.len(), 2);
        assert!(result.tracks.iter().any(|t| t.preserved && t.path.contains("owned.mp3")));
        assert!(result.playlists.iter().any(|p| p.device_only));
        std::fs::write(db.join("export.pdb"), &result.export_pdb).unwrap();
        std::fs::write(db.join("exportExt.pdb"), &result.export_ext_pdb).unwrap();
        std::fs::write(db.join("exportLibrary.db"), &result.export_library_db).unwrap();
        assert!(crate::verify_databases(server.path()).unwrap().is_ok());
        let library = crate::device_library::read(server.path(), crate::device_library::Format::OneLibrary).unwrap();
        assert!(library.tracks.iter().any(|t| t.title == "USB owned"));
        assert!(library.nodes.iter().any(|n| n.name == "USB set"));
    }

    #[test]
    fn matched_device_track_keeps_its_path_without_a_second_audio_row() {
        let original = tempfile::tempdir().unwrap();
        let source = original.path().join("owned.mp3");
        std::fs::write(&source, b"owned audio").unwrap();
        let device = original.path().join("device");
        crate::export(&device, &[SourceTrack {
            id: 17, source_path: source, title: "Owned".into(), file_size: 11,
            ..Default::default()
        }], &[]).unwrap();
        let server = tempfile::tempdir().unwrap();
        let db = server.path().join("PIONEER/rekordbox");
        std::fs::create_dir_all(&db).unwrap();
        for name in ["export.pdb", "exportExt.pdb", "exportLibrary.db"] {
            std::fs::copy(device.join("PIONEER/rekordbox").join(name), db.join(name)).unwrap();
        }
        let before = snapshot::Snapshot::read_at(server.path(), "PIONEER").unwrap();
        let existing = reconcile::capture_sources(server.path(), &before).unwrap();
        let (&device_id, row) = existing.iter().next().unwrap();
        let original_path = row.device.as_ref().unwrap().audio.clone();
        let plan = export_plan(server.path(), &[SourceTrack {
            id: 91, source_path: server.path().join("absent.mp3"),
            planned_audio_path: Some("/Contents/new/absent.mp3".into()),
            existing_device_id: Some(device_id), title: "Owned".into(), file_size: 11,
            ..Default::default()
        }], &[], &BTreeSet::new()).unwrap();
        assert_eq!(plan.tracks.len(), 1);
        assert_eq!(plan.tracks[0].path, original_path);
        assert!(plan.tracks[0].preserved);
    }

    #[test]
    fn matched_playlist_reuses_its_device_id_on_repeat_plans() {
        let original = tempfile::tempdir().unwrap();
        let source = original.path().join("owned.mp3");
        std::fs::write(&source, b"owned audio").unwrap();
        let device = original.path().join("device");
        crate::export(&device, &[SourceTrack {
            id: 17, source_path: source, title: "Owned".into(), file_size: 11,
            ..Default::default()
        }], &[SourcePlaylist {
            id: 17, name: "Owned set".into(), track_indices: vec![0], ..Default::default()
        }]).unwrap();
        let server = tempfile::tempdir().unwrap();
        let db = server.path().join("PIONEER/rekordbox");
        std::fs::create_dir_all(&db).unwrap();
        for name in ["export.pdb", "exportExt.pdb", "exportLibrary.db"] {
            std::fs::copy(device.join("PIONEER/rekordbox").join(name), db.join(name)).unwrap();
        }
        let before = snapshot::Snapshot::read_at(server.path(), "PIONEER").unwrap();
        let track_id = before.legacy.as_ref().unwrap().tracks[0].id;
        let playlist_id = before.legacy.as_ref().unwrap().playlists[0].id;
        let mut selected = [SourceTrack {
            id: 0x7EDC_BA98_7654_3210, source_path: server.path().join("absent.mp3"),
            planned_audio_path: Some("/Contents/new/absent.mp3".into()),
            existing_device_id: Some(track_id), title: "Owned".into(), file_size: 11,
            ..Default::default()
        }];
        let playlists = [SourcePlaylist {
            id: 91, device_id: playlist_id, name: "Owned set".into(),
            track_indices: vec![0], ..Default::default()
        }];
        for attempt in 0..2 {
            if attempt == 1 {
                selected[0].existing_device_id = None;
                selected[0].title = "Owned renamed".into();
            }
            let plan = export_plan(server.path(), &selected, &playlists, &BTreeSet::new()).unwrap();
            assert_eq!(plan.tracks.len(), 1);
            assert_eq!(plan.playlists.len(), 1);
            assert_eq!(plan.playlists[0].device_id, playlist_id);
            std::fs::write(db.join("export.pdb"), &plan.export_pdb).unwrap();
            std::fs::write(db.join("exportExt.pdb"), &plan.export_ext_pdb).unwrap();
            std::fs::write(db.join("exportLibrary.db"), &plan.export_library_db).unwrap();
            assert!(crate::verify_databases(server.path()).unwrap().is_ok());
        }
    }

    #[test]
    fn removes_only_unreferenced_owned_tracks() {
        let original = tempfile::tempdir().unwrap();
        let device = original.path().join("device");
        let sources: Vec<_> = (1..=4).map(|id| {
            let path = original.path().join(format!("{id}.mp3"));
            std::fs::write(&path, vec![id as u8; 16]).unwrap();
            SourceTrack { id, source_path: path, title: format!("Track {id}"), file_size: 16, ..Default::default() }
        }).collect();
        crate::export(&device, &sources, &[
            SourcePlaylist { id: 10, name: "Set".into(), track_indices: vec![0, 1], ..Default::default() },
            SourcePlaylist { id: 20, name: "Other".into(), track_indices: vec![2], ..Default::default() },
        ]).unwrap();
        let server = tempfile::tempdir().unwrap();
        let db = server.path().join("PIONEER/rekordbox");
        std::fs::create_dir_all(&db).unwrap();
        for name in ["export.pdb", "exportExt.pdb", "exportLibrary.db"] {
            std::fs::copy(device.join("PIONEER/rekordbox").join(name), db.join(name)).unwrap();
        }
        let before = snapshot::Snapshot::read_at(server.path(), "PIONEER").unwrap();
        let source_ids: BTreeMap<_, _> = before.legacy.as_ref().unwrap().tracks.iter()
            .map(|track| (track.title.clone(), track.id)).collect();
        let set_id = before.legacy.as_ref().unwrap().playlists.iter().find(|p| p.name == "Set").unwrap().id;
        let removed_id = source_ids["Track 2"];
        let plan = export_plan(server.path(), &[SourceTrack {
            id: 91, source_path: server.path().join("absent.mp3"),
            planned_audio_path: Some("/Contents/new/absent.mp3".into()),
            existing_device_id: Some(source_ids["Track 1"]),
            title: "Track 1".into(), file_size: 16, ..Default::default()
        }], &[SourcePlaylist { id: 10, device_id: set_id, name: "Set".into(), track_indices: vec![0], ..Default::default() }],
            &BTreeSet::from([removed_id, source_ids["Track 3"]])).unwrap();
        assert_eq!(plan.removed.len(), 1);
        assert_eq!(plan.removed[0].device_id, removed_id);
        assert_eq!(plan.tracks.len(), 3);
        assert!(plan.tracks.iter().any(|track| track.device_id == source_ids["Track 4"]));
        assert!(plan.tracks.iter().any(|track| track.device_id == source_ids["Track 3"]));
        for (name, bytes) in [("export.pdb", &plan.export_pdb), ("exportExt.pdb", &plan.export_ext_pdb), ("exportLibrary.db", &plan.export_library_db)] {
            std::fs::write(db.join(name), bytes).unwrap();
        }
        assert!(crate::verify_databases(server.path()).unwrap().is_ok());
    }

    #[test]
    fn legacy_only_baseline_gains_one_library_without_losing_its_tracks() {
        let original = tempfile::tempdir().unwrap();
        let source = original.path().join("owned.mp3");
        std::fs::write(&source, b"owned audio").unwrap();
        let device = original.path().join("device");
        crate::export(&device, &[SourceTrack {
            id: 17, source_path: source, title: "USB owned".into(), file_size: 11,
            ..Default::default()
        }], &[]).unwrap();
        let server = tempfile::tempdir().unwrap();
        let db = server.path().join("PIONEER/rekordbox");
        std::fs::create_dir_all(&db).unwrap();
        std::fs::copy(device.join("PIONEER/rekordbox/export.pdb"), db.join("export.pdb")).unwrap();
        let plan = export_plan(server.path(), &[SourceTrack {
            id: 91, source_path: server.path().join("absent.mp3"),
            planned_audio_path: Some("/Contents/Artist/absent.mp3".into()),
            title: "New".into(), file_size: 2048, ..Default::default()
        }], &[], &BTreeSet::new()).unwrap();
        assert_eq!(plan.tracks.len(), 2);
        std::fs::write(db.join("export.pdb"), &plan.export_pdb).unwrap();
        std::fs::write(db.join("exportExt.pdb"), &plan.export_ext_pdb).unwrap();
        std::fs::write(db.join("exportLibrary.db"), &plan.export_library_db).unwrap();
        assert!(crate::verify_databases(server.path()).unwrap().is_ok());
        let library = crate::device_library::read(server.path(), crate::device_library::Format::OneLibrary).unwrap();
        assert!(library.tracks.iter().any(|track| track.title == "USB owned"));
    }
}
