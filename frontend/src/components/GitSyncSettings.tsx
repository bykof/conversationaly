import React, { useCallback, useEffect, useState } from 'react';
import { FolderOpen, RefreshCw } from 'lucide-react';
import { invoke } from '@tauri-apps/api/core';
import { toast } from 'sonner';
import { Dialog, DialogContent, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';

interface GitSyncSettingsState {
  repoPath: string;
  remoteUrl: string;
  subfolder: string;
  token: string;
  authorName: string;
  authorEmail: string;
}

interface GitSyncStatus {
  configured: boolean;
  meetingCount: number;
  firstSync: boolean;
}

interface GitSyncOutcome {
  conflicts: string[];
  written: number;
  deleted: number;
  committed: boolean;
  pushed: boolean;
  message: string;
}

const EMPTY: GitSyncSettingsState = {
  repoPath: '',
  remoteUrl: '',
  subfolder: 'meetings',
  token: '',
  authorName: '',
  authorEmail: '',
};

const inputClass =
  'w-full px-3 py-2 text-sm border border-line rounded-md bg-surface focus:outline-none focus:ring-2 focus:ring-accent/40';

export function GitSyncSettings() {
  const [settings, setSettings] = useState<GitSyncSettingsState>(EMPTY);
  const [status, setStatus] = useState<GitSyncStatus | null>(null);
  const [loading, setLoading] = useState(true);
  const [syncing, setSyncing] = useState(false);
  const [conflicts, setConflicts] = useState<string[]>([]);
  const [keepOurs, setKeepOurs] = useState<Record<string, boolean>>({});

  const refreshStatus = useCallback(async () => {
    try {
      setStatus(await invoke<GitSyncStatus>('git_sync_status'));
    } catch (error) {
      console.error('Failed to load git sync status:', error);
    }
  }, []);

  useEffect(() => {
    (async () => {
      try {
        setSettings(await invoke<GitSyncSettingsState>('git_sync_get_settings'));
        await refreshStatus();
      } catch (error) {
        console.error('Failed to load git sync settings:', error);
      } finally {
        setLoading(false);
      }
    })();
  }, [refreshStatus]);

  const persist = async (next: GitSyncSettingsState) => {
    setSettings(next);
    try {
      await invoke('git_sync_set_settings', { settings: next });
      await refreshStatus();
    } catch (error) {
      toast.error('Could not save sync settings', {
        description: error instanceof Error ? error.message : String(error),
      });
    }
  };

  const pickFolder = async () => {
    const folder = await invoke<string | null>('git_sync_select_repo_folder');
    if (folder) await persist({ ...settings, repoPath: folder });
  };

  // `resolutions` is undefined on the first pass: the backend then reports
  // conflicts instead of writing anything.
  const runSync = async (resolutions?: Record<string, boolean>) => {
    setSyncing(true);
    try {
      const outcome = await invoke<GitSyncOutcome>('git_sync_run', { resolutions });
      if (outcome.conflicts.length > 0) {
        setConflicts(outcome.conflicts);
        setKeepOurs(Object.fromEntries(outcome.conflicts.map((path) => [path, true])));
        return;
      }
      setConflicts([]);
      toast.success(outcome.message, {
        description: `${outcome.written} file(s) written, ${outcome.deleted} folder(s) removed${
          outcome.pushed ? ', pushed' : ''
        }`,
      });
      await refreshStatus();
    } catch (error) {
      toast.error('Sync failed', {
        description: error instanceof Error ? error.message : String(error),
      });
    } finally {
      setSyncing(false);
    }
  };

  const startSync = async () => {
    if (status?.firstSync && status.meetingCount > 0) {
      const ok = window.confirm(
        `${status.meetingCount} meeting(s) will be written to the repository. Continue?`,
      );
      if (!ok) return;
    }
    await runSync();
  };

  if (loading) {
    return (
      <div className="animate-pulse">
        <div className="h-4 bg-ink/10 rounded w-1/4 mb-4" />
        <div className="h-8 bg-ink/10 rounded mb-4" />
      </div>
    );
  }

  return (
    <div className="space-y-6">
      <div>
        <h3 className="text-lg font-semibold mb-4">Git Sync</h3>
        <p className="text-sm text-ink-muted mb-6">
          Writes each meeting&apos;s <code>summary.md</code> and <code>transcript.md</code> into a git
          repository and pushes them. Audio, API keys and models stay on this machine.
        </p>
      </div>

      <div className="p-4 border rounded-lg bg-sunken space-y-3">
        <div className="font-medium">Repository folder</div>
        <div className="text-sm text-ink-muted break-all">
          {settings.repoPath || 'No folder selected'}
        </div>
        <button
          onClick={pickFolder}
          className="flex items-center gap-2 px-3 py-2 text-sm border border-line rounded-md hover:bg-surface transition-colors"
        >
          <FolderOpen className="w-4 h-4" />
          Choose folder
        </button>
        <div className="text-xs text-ink-muted">
          An existing clone, or an empty folder — an empty one is initialised with the remote below.
        </div>
      </div>

      <div className="space-y-4">
        <label className="block">
          <span className="text-sm font-medium">Remote URL</span>
          <input
            className={inputClass}
            placeholder="https://username@github.com/team/notes.git"
            value={settings.remoteUrl}
            onChange={(e) => setSettings({ ...settings, remoteUrl: e.target.value })}
            onBlur={() => persist(settings)}
          />
          <span className="text-xs text-ink-muted">
            Only used for an empty folder; an existing clone keeps its own <code>origin</code>. Put
            your username in the URL — GitLab and Gitea need it.
          </span>
        </label>

        <label className="block">
          <span className="text-sm font-medium">Subfolder in the repository</span>
          <input
            className={inputClass}
            placeholder="meetings"
            value={settings.subfolder}
            onChange={(e) => setSettings({ ...settings, subfolder: e.target.value })}
            onBlur={() => persist(settings)}
          />
        </label>

        <label className="block">
          <span className="text-sm font-medium">Personal access token</span>
          <input
            className={inputClass}
            type="password"
            placeholder="ghp_…"
            value={settings.token}
            onChange={(e) => setSettings({ ...settings, token: e.target.value })}
            onBlur={() => persist(settings)}
          />
          <span className="text-xs text-ink-muted">Needs write access to the repository.</span>
        </label>

        <div className="grid grid-cols-2 gap-4">
          <label className="block">
            <span className="text-sm font-medium">Author name</span>
            <input
              className={inputClass}
              placeholder="from ~/.gitconfig"
              value={settings.authorName}
              onChange={(e) => setSettings({ ...settings, authorName: e.target.value })}
              onBlur={() => persist(settings)}
            />
          </label>
          <label className="block">
            <span className="text-sm font-medium">Author email</span>
            <input
              className={inputClass}
              placeholder="from ~/.gitconfig"
              value={settings.authorEmail}
              onChange={(e) => setSettings({ ...settings, authorEmail: e.target.value })}
              onBlur={() => persist(settings)}
            />
          </label>
        </div>
      </div>

      <div className="flex items-center justify-between p-4 border rounded-lg">
        <div className="text-sm text-ink-muted">
          {status
            ? `${status.meetingCount} meeting(s) ready${status.firstSync ? ' · never synced' : ''}`
            : 'Status unavailable'}
        </div>
        <button
          onClick={startSync}
          disabled={syncing || !settings.repoPath}
          className="flex items-center gap-2 px-4 py-2 text-sm border border-line rounded-md hover:bg-sunken transition-colors disabled:opacity-50"
        >
          <RefreshCw className={`w-4 h-4 ${syncing ? 'animate-spin' : ''}`} />
          {syncing ? 'Syncing…' : 'Sync now'}
        </button>
      </div>

      <Dialog open={conflicts.length > 0} onOpenChange={(open) => !open && setConflicts([])}>
        <DialogContent aria-describedby={undefined}>
          <DialogHeader>
            <DialogTitle>Someone else changed these files</DialogTitle>
          </DialogHeader>
          <p className="text-sm text-ink-muted">
            Pick which version wins. Nothing is lost either way — the other version stays in the
            repository&apos;s history.
          </p>
          <div className="max-h-64 overflow-y-auto space-y-2">
            {conflicts.map((path) => (
              <div key={path} className="flex items-center justify-between gap-3 text-sm">
                <span className="break-all">{path}</span>
                <select
                  className="border border-line rounded-md px-2 py-1 bg-surface"
                  value={keepOurs[path] ? 'ours' : 'theirs'}
                  onChange={(e) =>
                    setKeepOurs({ ...keepOurs, [path]: e.target.value === 'ours' })
                  }
                >
                  <option value="ours">Overwrite</option>
                  <option value="theirs">Keep repository</option>
                </select>
              </div>
            ))}
          </div>
          <DialogFooter>
            <button
              onClick={() => setConflicts([])}
              className="px-4 py-2 text-sm border border-line rounded-md hover:bg-sunken"
            >
              Cancel
            </button>
            <button
              onClick={() => {
                const resolutions = keepOurs;
                setConflicts([]);
                void runSync(resolutions);
              }}
              className="px-4 py-2 text-sm border border-line rounded-md hover:bg-sunken"
            >
              Apply and sync
            </button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
