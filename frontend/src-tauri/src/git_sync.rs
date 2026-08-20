//! Markdown-Sync der Meeting-Ordner in ein git-Repo.
//!
//! Einbahnstraße: wir kopieren `summary.md` und `transcript.md` aus den
//! Meeting-Ordnern in ein Repo, committen und pushen. Fremde Meetings werden
//! nie importiert und tauchen nirgends in der App auf.
//!
//! Es wird bewusst *nichts* gerendert: beide Dateien schreibt die App ohnehin
//! schon in den Meeting-Ordner (`audio/common.rs::write_transcript_md`,
//! `summary/metadata.rs::write_summary_md`). Ein zweiter Renderer hier wäre
//! ein zweiter Wahrheitsstand.

use anyhow::{anyhow, bail, Context, Result};
use git2::{
    build::CheckoutBuilder, Cred, FetchOptions, IndexAddOption, PushOptions, RemoteCallbacks,
    Repository, Signature, StatusOptions,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Runtime};
use tauri_plugin_store::StoreExt;

use crate::state::AppState;

const STORE_FILE: &str = "git_sync.json";
const STORE_KEY: &str = "settings";

/// Was aus einem Meeting-Ordner ins Repo geht. Audio, `transcripts.json`,
/// `metadata.json` und `.checkpoints/` bleiben draußen — nur Text.
const SYNCED_FILES: [&str; 2] = ["summary.md", "transcript.md"];

// ponytail: `notes.md` fehlt hier mit Absicht. Die Tabelle `meeting_notes`
// existiert, wird aber von niemandem beschrieben — die Datei wäre immer leer.
// Sobald der Notiz-Editor wirklich speichert, kommt "notes.md" in SYNCED_FILES.

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct GitSyncSettings {
    /// Arbeitskopie: ein bestehender Klon oder ein leerer Ordner.
    pub repo_path: String,
    /// Nur benutzt, wenn `repo_path` noch kein Repo ist. Sonst gilt `origin`.
    pub remote_url: String,
    /// Unterordner im Repo — es ist der Vault des Nutzers, da liegt anderes daneben.
    pub subfolder: String,
    /// Personal Access Token für HTTPS.
    pub token: String,
    /// Fallback, wenn `~/.gitconfig` keinen Autor kennt.
    pub author_name: String,
    pub author_email: String,
    /// HEAD nach dem letzten Sync. Ohne den würde der Konfliktdialog bei jeder
    /// eigenen Änderung feuern statt nur bei fremden.
    pub last_synced_commit: String,
    /// Ordnernamen, die *wir* zuletzt geschrieben haben. Nur die dürfen wir
    /// löschen — alles andere im Unterordner gehört jemand anderem.
    pub synced_folders: Vec<String>,
}

impl Default for GitSyncSettings {
    fn default() -> Self {
        Self {
            repo_path: String::new(),
            remote_url: String::new(),
            subfolder: "meetings".to_string(),
            token: String::new(),
            author_name: String::new(),
            author_email: String::new(),
            last_synced_commit: String::new(),
            synced_folders: Vec::new(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitSyncStatus {
    pub configured: bool,
    /// Meetings, die einen Ordner mit mindestens einer Markdown-Datei haben.
    pub meeting_count: usize,
    pub first_sync: bool,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitSyncOutcome {
    /// Repo-relative Pfade, die jemand anders geändert hat. Nicht leer heißt:
    /// nichts wurde geschrieben, der Nutzer muss pro Datei entscheiden.
    pub conflicts: Vec<String>,
    pub written: usize,
    pub deleted: usize,
    pub committed: bool,
    pub pushed: bool,
    pub message: String,
}

/// Ein Meeting-Ordner, wie er auf der Platte liegt.
struct MeetingFolder {
    /// Der Ordnername, unverändert übernommen (`Titel_2026-08-20_14-30`).
    name: String,
    path: PathBuf,
}

// ---------------------------------------------------------------- settings io

pub fn load_settings<R: Runtime>(app: &AppHandle<R>) -> GitSyncSettings {
    let Ok(store) = app.store(STORE_FILE) else {
        return GitSyncSettings::default();
    };
    store
        .get(STORE_KEY)
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

pub fn save_settings<R: Runtime>(app: &AppHandle<R>, settings: &GitSyncSettings) -> Result<()> {
    let store = app
        .store(STORE_FILE)
        .map_err(|e| anyhow!("Settings store unavailable: {}", e))?;
    store.set(STORE_KEY, serde_json::to_value(settings)?);
    store.save().map_err(|e| anyhow!("Settings store not writable: {}", e))
}

// ------------------------------------------------------------------ meetings

async fn meeting_folders(pool: &sqlx::SqlitePool) -> Result<Vec<MeetingFolder>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT folder_path FROM meetings \
         WHERE folder_path IS NOT NULL AND TRIM(folder_path) != '' \
         ORDER BY created_at",
    )
    .fetch_all(pool)
    .await
    .context("Could not read meetings")?;

    Ok(rows
        .into_iter()
        .filter_map(|(raw,)| {
            let path = PathBuf::from(raw);
            let name = path.file_name()?.to_string_lossy().to_string();
            // Ein Ordner ohne Markdown hat nichts beizutragen; ihn trotzdem
            // anzulegen hinterlässt leere Verzeichnisse im Repo.
            SYNCED_FILES
                .iter()
                .any(|f| path.join(f).is_file())
                .then_some(MeetingFolder { name, path })
        })
        .collect())
}

// ----------------------------------------------------------------- git plumbing

fn callbacks(token: String) -> RemoteCallbacks<'static> {
    let mut cb = RemoteCallbacks::new();
    cb.credentials(move |_url, username_from_url, _allowed| {
        if token.is_empty() {
            return Err(git2::Error::from_str(
                "No personal access token configured",
            ));
        }
        // Der Benutzername steckt bei GitLab/Gitea in der Remote-URL
        // (https://user@host/...). GitHub ist er egal, solange das Passwort
        // ein PAT ist.
        Cred::userpass_plaintext(username_from_url.unwrap_or("git"), &token)
    });
    cb
}

fn open_or_init(settings: &GitSyncSettings) -> Result<Repository> {
    let path = Path::new(&settings.repo_path);
    if !path.is_dir() {
        bail!("Folder does not exist: {}", settings.repo_path);
    }
    if let Ok(repo) = Repository::open(path) {
        return Ok(repo);
    }
    let repo = Repository::init(path).context("git init failed")?;
    if !settings.remote_url.trim().is_empty() {
        repo.remote("origin", settings.remote_url.trim())
            .context("Could not set remote")?;
    }
    Ok(repo)
}

/// Wir schreiben in ein Repo, das dem Nutzer gehört und in dem anderes liegt.
/// Uncommittete Fremdänderungen wären nach einem Fast-Forward weg.
fn ensure_clean(repo: &Repository) -> Result<()> {
    let mut opts = StatusOptions::new();
    opts.include_untracked(false).include_ignored(false);
    let statuses = repo.statuses(Some(&mut opts))?;
    if statuses.is_empty() {
        return Ok(());
    }
    bail!(
        "The repository has {} uncommitted change(s). Commit or discard them first.",
        statuses.len()
    )
}

fn current_branch(repo: &Repository) -> String {
    repo.head()
        .ok()
        .and_then(|h| h.shorthand().map(str::to_string))
        .unwrap_or_else(|| "main".to_string())
}

/// Fetch und Fast-Forward. Bei divergierter Historie brechen wir ab, statt zu
/// mergen — ein halbgarer Merge in fremden Vaults ist teurer als eine Meldung.
fn pull(repo: &Repository, settings: &GitSyncSettings, branch: &str) -> Result<()> {
    let Ok(mut remote) = repo.find_remote("origin") else {
        return Ok(()); // Kein Remote: rein lokale Commits, das ist erlaubt.
    };

    let mut fo = FetchOptions::new();
    fo.remote_callbacks(callbacks(settings.token.clone()));
    // Wildcard-Refspec, damit ein noch leeres Remote kein Fehler ist.
    remote
        .fetch(&["+refs/heads/*:refs/remotes/origin/*"], Some(&mut fo), None)
        .context("Fetch failed — check the token and the remote URL")?;

    let Ok(remote_ref) = repo.find_reference(&format!("refs/remotes/origin/{}", branch)) else {
        return Ok(()); // Branch gibt es dort noch nicht.
    };
    let incoming = repo.reference_to_annotated_commit(&remote_ref)?;
    let (analysis, _) = repo.merge_analysis(&[&incoming])?;

    if analysis.is_up_to_date() {
        return Ok(());
    }
    if !analysis.is_fast_forward() && !analysis.is_unborn() {
        bail!(
            "Local repository and remote have diverged. Reconcile them with git \
             (merge or rebase), then sync again."
        );
    }

    let refname = format!("refs/heads/{}", branch);
    match repo.find_reference(&refname) {
        Ok(mut r) => {
            r.set_target(incoming.id(), "git-sync: fast-forward")?;
        }
        Err(_) => {
            repo.reference(&refname, incoming.id(), true, "git-sync: initial")?;
        }
    }
    repo.set_head(&refname)?;
    repo.checkout_head(Some(CheckoutBuilder::default().force()))?;
    Ok(())
}

/// Pfade, die sich seit dem letzten Sync geändert haben.
///
/// `None` heißt „kein verwertbarer Bezugspunkt" (erster Sync, frisch geklont,
/// umgeschriebene Historie) — dann gilt jede vorhandene abweichende Datei als
/// fremd.
fn changed_since(repo: &Repository, last: &str) -> Result<Option<BTreeSet<String>>> {
    if last.is_empty() {
        return Ok(None);
    }
    let Ok(oid) = git2::Oid::from_str(last) else {
        return Ok(None);
    };
    let (Ok(old), Ok(head)) = (
        repo.find_commit(oid),
        repo.head().and_then(|h| h.peel_to_commit()),
    ) else {
        return Ok(None);
    };

    let diff = repo.diff_tree_to_tree(Some(&old.tree()?), Some(&head.tree()?), None)?;
    let mut out = BTreeSet::new();
    diff.foreach(
        &mut |delta, _| {
            if let Some(p) = delta.new_file().path().or_else(|| delta.old_file().path()) {
                out.insert(p.to_string_lossy().to_string());
            }
            true
        },
        None,
        None,
        None,
    )?;
    Ok(Some(out))
}

// ----------------------------------------------------------------- planning

/// Was wir schreiben wollen: repo-relativer Pfad -> Inhalt.
fn desired_files(folders: &[MeetingFolder], subfolder: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for folder in folders {
        for file in SYNCED_FILES {
            let src = folder.path.join(file);
            let Ok(content) = std::fs::read_to_string(&src) else {
                continue;
            };
            out.push((format!("{}/{}/{}", subfolder, folder.name, file), content));
        }
    }
    out
}

/// Eine Datei ist ein Konflikt, wenn sie im Repo anders aussieht als das, was
/// wir schreiben würden, *und* jemand anders sie angefasst hat.
fn plan_conflicts(
    workdir: &Path,
    desired: &[(String, String)],
    changed: &Option<BTreeSet<String>>,
) -> Vec<String> {
    desired
        .iter()
        .filter(|(rel, content)| {
            let existing = std::fs::read_to_string(workdir.join(rel)).ok();
            let differs = existing.as_deref() != Some(content.as_str());
            let touched_by_other = match changed {
                Some(set) => set.contains(rel),
                None => existing.is_some(),
            };
            differs && touched_by_other
        })
        .map(|(rel, _)| rel.clone())
        .collect()
}

/// Ordner, die wir beim letzten Mal geschrieben haben und die es lokal nicht
/// mehr gibt. Fremde Ordner stehen nie in `synced_folders` und bleiben liegen.
fn stale_folders(synced: &[String], current: &BTreeSet<String>) -> Vec<String> {
    synced
        .iter()
        .filter(|name| !current.contains(*name))
        .cloned()
        .collect()
}

fn signature(repo: &Repository, settings: &GitSyncSettings) -> Result<Signature<'static>> {
    // `repo.signature()` liest user.name/user.email aus der git-Konfiguration,
    // inklusive ~/.gitconfig. Die Felder in den Settings sind nur der Fallback.
    if let Ok(sig) = repo.signature() {
        return Ok(Signature::now(
            &sig.name().unwrap_or("Conversationaly").to_string(),
            &sig.email().unwrap_or("conversationaly@localhost").to_string(),
        )?);
    }
    let (name, email) = (settings.author_name.trim(), settings.author_email.trim());
    if name.is_empty() || email.is_empty() {
        bail!(
            "No git author found. Set user.name/user.email in ~/.gitconfig, or fill in \
             name and email in the sync settings."
        );
    }
    Ok(Signature::now(name, email)?)
}

// ------------------------------------------------------------------- the sync

/// Der blockierende Teil. Läuft in `spawn_blocking`, weil git2 synchron ist.
fn sync_blocking(
    mut settings: GitSyncSettings,
    folders: Vec<MeetingFolder>,
    resolutions: Option<BTreeMap<String, bool>>,
) -> Result<(GitSyncOutcome, GitSyncSettings)> {
    let repo = open_or_init(&settings)?;
    ensure_clean(&repo)?;
    let branch = current_branch(&repo);
    pull(&repo, &settings, &branch)?;

    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow!("Bare repositories are not supported"))?
        .to_path_buf();

    let subfolder = settings.subfolder.trim().trim_matches('/');
    let subfolder = if subfolder.is_empty() { "meetings" } else { subfolder };

    let desired = desired_files(&folders, subfolder);
    let changed = changed_since(&repo, &settings.last_synced_commit)?;
    let conflicts = plan_conflicts(&workdir, &desired, &changed);

    // Erster Durchlauf mit Konflikten: nichts anfassen, den Nutzer fragen.
    let Some(resolutions) = resolutions.or_else(|| conflicts.is_empty().then(BTreeMap::new)) else {
        return Ok((
            GitSyncOutcome {
                message: format!("{} file(s) need a decision.", conflicts.len()),
                conflicts,
                ..Default::default()
            },
            settings,
        ));
    };

    let mut written = 0usize;
    for (rel, content) in &desired {
        // Nicht aufgelöste Konflikte gelten als "Repo behalten".
        if conflicts.contains(rel) && !resolutions.get(rel).copied().unwrap_or(false) {
            continue;
        }
        let dest = workdir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Could not create folder: {}", parent.display()))?;
        }
        std::fs::write(&dest, content)
            .with_context(|| format!("Could not write file: {}", dest.display()))?;
        written += 1;
    }

    let current: BTreeSet<String> = folders.iter().map(|f| f.name.clone()).collect();
    let mut deleted = 0usize;
    for name in stale_folders(&settings.synced_folders, &current) {
        let dir = workdir.join(subfolder).join(&name);
        if dir.is_dir() && std::fs::remove_dir_all(&dir).is_ok() {
            deleted += 1;
        }
    }

    let mut index = repo.index()?;
    index.add_all([subfolder].iter(), IndexAddOption::DEFAULT, None)?;
    index.update_all([subfolder].iter(), None)?;
    index.write()?;
    let tree = repo.find_tree(index.write_tree()?)?;

    let head = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
    let unchanged = head.as_ref().map(|c| c.tree_id()) == Some(tree.id());

    let mut outcome = GitSyncOutcome {
        conflicts: Vec::new(),
        written,
        deleted,
        ..Default::default()
    };

    if unchanged {
        outcome.message = "Nothing to do — the repository is up to date.".to_string();
    } else {
        let sig = signature(&repo, &settings)?;
        let parents: Vec<&git2::Commit> = head.iter().collect();
        let message = format!("conversationaly: {} meetings", folders.len());
        let commit = repo.commit(Some("HEAD"), &sig, &sig, &message, &tree, &parents)?;
        outcome.committed = true;
        settings.last_synced_commit = commit.to_string();
        outcome.message = message;
    }

    settings.synced_folders = current.into_iter().collect();

    if let Ok(mut remote) = repo.find_remote("origin") {
        if outcome.committed {
            let mut po = PushOptions::new();
            po.remote_callbacks(callbacks(settings.token.clone()));
            remote
                .push(
                    &[format!("refs/heads/{b}:refs/heads/{b}", b = branch)],
                    Some(&mut po),
                )
                .context("Push failed — check the token and your write access")?;
            outcome.pushed = true;
        }
    } else {
        outcome.message.push_str(" (no remote — committed locally only)");
    }

    Ok((outcome, settings))
}

// ------------------------------------------------------------------ commands

#[tauri::command]
pub async fn git_sync_get_settings<R: Runtime>(app: AppHandle<R>) -> Result<GitSyncSettings, String> {
    Ok(load_settings(&app))
}

#[tauri::command]
pub async fn git_sync_set_settings<R: Runtime>(
    app: AppHandle<R>,
    settings: GitSyncSettings,
) -> Result<(), String> {
    // Der Nutzer bearbeitet nie den Sync-Zustand — den behalten wir.
    let previous = load_settings(&app);
    let merged = GitSyncSettings {
        last_synced_commit: previous.last_synced_commit,
        synced_folders: previous.synced_folders,
        ..settings
    };
    save_settings(&app, &merged).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn git_sync_select_repo_folder<R: Runtime>(app: AppHandle<R>) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt;
    Ok(app.dialog().file().blocking_pick_folder().map(|p| p.to_string()))
}

#[tauri::command]
pub async fn git_sync_status<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
) -> Result<GitSyncStatus, String> {
    let settings = load_settings(&app);
    let folders = meeting_folders(state.db_manager.pool())
        .await
        .map_err(|e| e.to_string())?;
    Ok(GitSyncStatus {
        configured: !settings.repo_path.trim().is_empty(),
        meeting_count: folders.len(),
        first_sync: settings.last_synced_commit.is_empty(),
    })
}

/// Ein Klick auf „Sync". Ohne `resolutions` bricht der Lauf bei Konflikten ab
/// und meldet sie; der zweite Aufruf trägt die Entscheidungen nach
/// (`true` = unsere Version gewinnt).
#[tauri::command]
pub async fn git_sync_run<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    resolutions: Option<BTreeMap<String, bool>>,
) -> Result<GitSyncOutcome, String> {
    // libgit2 und die Transkription um dieselbe Platte streiten zu lassen ist
    // die Klasse Bug, die man nachts sucht.
    if crate::audio::recording_commands::is_recording().await {
        return Err("Syncing is disabled while recording.".to_string());
    }

    let settings = load_settings(&app);
    if settings.repo_path.trim().is_empty() {
        return Err("No repository folder configured.".to_string());
    }

    let folders = meeting_folders(state.db_manager.pool())
        .await
        .map_err(|e| e.to_string())?;

    let (outcome, updated) = tokio::task::spawn_blocking(move || {
        sync_blocking(settings, folders, resolutions)
    })
    .await
    .map_err(|e| format!("Sync task crashed: {}", e))?
    .map_err(|e| e.to_string())?;

    save_settings(&app, &updated).map_err(|e| e.to_string())?;
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn stale_folders_spares_foreign_ones() {
        let synced = vec!["mine_a".to_string(), "mine_b".to_string()];
        let current: BTreeSet<String> = ["mine_a".to_string()].into_iter().collect();
        // mine_b wurde lokal gelöscht, "teammate_c" stand nie in synced_folders.
        assert_eq!(stale_folders(&synced, &current), vec!["mine_b".to_string()]);
    }

    #[test]
    fn first_sync_flags_only_differing_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("meetings/a/summary.md"), "fremd");
        write(&dir.path().join("meetings/b/summary.md"), "gleich");

        let desired = vec![
            ("meetings/a/summary.md".to_string(), "meins".to_string()),
            ("meetings/b/summary.md".to_string(), "gleich".to_string()),
            ("meetings/c/summary.md".to_string(), "neu".to_string()),
        ];

        // Kein Bezugspunkt: vorhanden + abweichend = Konflikt.
        assert_eq!(
            plan_conflicts(dir.path(), &desired, &None),
            vec!["meetings/a/summary.md".to_string()]
        );

        // Mit Bezugspunkt zählt nur, was der Diff nennt.
        let changed = Some(BTreeSet::from(["meetings/b/summary.md".to_string()]));
        assert!(plan_conflicts(dir.path(), &desired, &changed).is_empty());
    }

    #[test]
    fn changed_since_reports_the_other_sides_commit() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        let sig = Signature::now("Test", "test@localhost").unwrap();

        let commit_file = |repo: &Repository, rel: &str, body: &str, parent: Option<git2::Oid>| {
            write(&dir.path().join(rel), body);
            let mut index = repo.index().unwrap();
            index.add_all(["."].iter(), IndexAddOption::DEFAULT, None).unwrap();
            index.write().unwrap();
            let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
            let parents: Vec<git2::Commit> =
                parent.into_iter().map(|id| repo.find_commit(id).unwrap()).collect();
            let refs: Vec<&git2::Commit> = parents.iter().collect();
            repo.commit(Some("HEAD"), &sig, &sig, "c", &tree, &refs).unwrap()
        };

        let base = commit_file(&repo, "meetings/a/summary.md", "v1", None);
        commit_file(&repo, "meetings/a/summary.md", "v2", Some(base));

        let changed = changed_since(&repo, &base.to_string()).unwrap().unwrap();
        assert!(changed.contains("meetings/a/summary.md"));

        // Unbekannter Bezugspunkt darf nicht panicken, sondern fällt auf None.
        assert!(changed_since(&repo, "deadbeef").unwrap().is_none());
    }
}
