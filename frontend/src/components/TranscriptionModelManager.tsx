'use client';

/**
 * Transcription model manager.
 *
 * Replaces WhisperModelManager and ParakeetModelManager, which were the same
 * component against two engines. One engine now means one catalog and one list.
 */

import { useCallback, useEffect, useState } from 'react';
import { listen } from '@tauri-apps/api/event';
import {
  MODEL_SORT_LABELS,
  ModelInfo,
  ModelSort,
  TranscribeAPI,
  corruptedSizeMb,
  downloadProgress,
  formatFileSize,
  getModelIcon,
  getModelLabel,
  getModelUseTag,
  isDownloading,
  sortModels,
} from '@/lib/transcribe';
import { Button } from '@/components/ui/button';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select';
import { Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { Search } from 'lucide-react';
import { Switch } from '@/components/ui/switch';
import { languageHaystack, languageNames, languagesSummary } from '@/lib/languages';
import { cn } from '@/lib/utils';

interface Props {
  selectedModel?: string;
  onModelSelect?: (modelName: string) => void;
}

export default function TranscriptionModelManager({ selectedModel, onModelSelect }: Props) {
  const [models, setModels] = useState<ModelInfo[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [sort, setSort] = useState<ModelSort>('catalog');
  const [query, setQuery] = useState('');
  const [installedOnly, setInstalledOnly] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setModels(await TranscribeAPI.getAvailableModels());
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    refresh();
  }, [refresh]);

  // Download progress arrives as events; re-read the list on each terminal event
  // rather than mirroring per-model progress into local state.
  useEffect(() => {
    const unlisteners = [
      listen<{ modelName: string; progress: number }>('model-download-progress', (e) => {
        setModels((prev) =>
          prev.map((m) =>
            m.name === e.payload.modelName
              ? { ...m, status: { Downloading: { progress: e.payload.progress } } }
              : m
          )
        );
      }),
      listen('model-download-complete', () => {
        setBusy(null);
        refresh();
      }),
      listen<{ error: string }>('model-download-error', (e) => {
        setBusy(null);
        setError(e.payload.error);
        refresh();
      }),
    ];
    return () => {
      unlisteners.forEach((p) => p.then((un) => un()));
    };
  }, [refresh]);

  const download = async (name: string) => {
    setBusy(name);
    setError(null);
    try {
      await TranscribeAPI.downloadModel(name);
    } catch (e) {
      setError(String(e));
      setBusy(null);
    }
  };

  const remove = async (name: string) => {
    try {
      await TranscribeAPI.deleteCorruptedModel(name);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const card = (model: ModelInfo) => {
    const available = model.status === 'Available';
    const downloading = isDownloading(model.status);
    const selected = selectedModel === model.name;
    // On disk but short of its catalog size — an interrupted download. It can
    // neither be used nor re-fetched silently, and it is still occupying the
    // disk, so the row has to say so and offer both ways out.
    const truncatedMb = corruptedSizeMb(model.status);

    // Selection is a brand border, never a filled surface — the fill is what
    // made a selected card read as a status callout. See /DESIGN.md.
    return (
      <div
        key={model.name}
        className={`rounded-lg border p-4 transition-colors ${
          selected ? 'border-brand' : 'border-line'
        }`}
      >
        <div className="flex items-start justify-between gap-4">
          <div className="min-w-0">
            <div className="flex flex-wrap items-center gap-2">
              <span>{getModelIcon(model.accuracy)}</span>
              <span className="font-medium">{getModelLabel(model.name)}</span>
              <span
                className={`rounded-full px-2 py-0.5 text-xs ${
                  model.streaming
                    ? 'bg-brand-soft text-brand-soft-ink'
                    : 'bg-warn-soft text-warn-ink'
                }`}
              >
                {getModelUseTag(model)}
              </span>
              {model.diarizes && (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <span className="cursor-default rounded-full border border-line px-2 py-0.5 text-xs text-ink-muted">
                      Labels speakers
                    </span>
                  </TooltipTrigger>
                  <TooltipContent side="bottom">
                    Marks who is speaking, as Speaker 1, Speaker 2, … You can rename
                    them once the meeting is saved.
                  </TooltipContent>
                </Tooltip>
              )}
              {model.recommended && (
                <span className="rounded-full bg-info-soft px-2 py-0.5 text-xs text-info-ink">
                  Recommended
                </span>
              )}
            </div>
            <p className="mt-1 text-sm text-ink-muted">{model.description}</p>
            <dl className="mt-2 flex flex-wrap gap-x-4 gap-y-1 text-xs text-ink-muted">
              <div className="flex gap-1">
                <dt>Quality</dt>
                <dd className="font-medium text-ink">{model.accuracy}</dd>
              </div>
              {model.wer !== null && (
                <div className="flex gap-1">
                  <dt>WER</dt>
                  <dd className="readout text-ink">
                    <Tooltip>
                      <TooltipTrigger className="cursor-help underline decoration-dotted underline-offset-2">
                        {model.wer.toFixed(2)}%
                      </TooltipTrigger>
                      {/* The set is in the tooltip, not the row, because the row has
                          to stay scannable across 86 models — but it is one hover
                          away, because 1.27% on English read speech and 8.40% on
                          Russian are not two points on the same scale. */}
                      <TooltipContent side="top">
                        Word error rate on {model.wer_set}, measured by transcribe.cpp.
                        Lower is better, and only comparable against other models on
                        the same set — not against your meetings.
                      </TooltipContent>
                    </Tooltip>
                  </dd>
                </div>
              )}
              <div className="flex gap-1">
                <dt>Speed</dt>
                <dd className="font-medium text-ink">{model.speed}</dd>
              </div>
              <div className="flex gap-1">
                <dt>Download</dt>
                <dd className="readout text-ink">{formatFileSize(model.size_mb)}</dd>
              </div>
              <div className="flex gap-1">
                <dt className="sr-only">Languages</dt>
                <dd>
                  <Tooltip>
                    <TooltipTrigger className="cursor-help underline decoration-dotted underline-offset-2">
                      {languagesSummary(model.languages)}
                    </TooltipTrigger>
                    <TooltipContent side="top" className="max-w-80">
                      {languageNames(model.languages).join(', ')}
                    </TooltipContent>
                  </Tooltip>
                </dd>
              </div>
            </dl>
          </div>

          <div className="flex shrink-0 items-center gap-2">
            {available && !selected && (
              <Button size="sm" onClick={() => onModelSelect?.(model.name)}>
                Use
              </Button>
            )}
            {available && selected && (
              <span className="text-sm font-medium text-brand">Selected</span>
            )}
            {!available && !downloading && (
              <Button
                size="sm"
                variant="outline"
                disabled={busy !== null}
                onClick={() => download(model.name)}
              >
                {truncatedMb === null ? 'Download' : 'Download again'}
              </Button>
            )}
            {(available || truncatedMb !== null) && (
              <Button size="sm" variant="ghost" onClick={() => remove(model.name)}>
                Delete
              </Button>
            )}
          </div>
        </div>

        {truncatedMb !== null && (
          <p className="mt-3 rounded-md bg-warn-soft px-3 py-2 text-xs text-warn-ink">
            Damaged download — {truncatedMb} MB on disk of {model.size_mb} MB. Download
            again to repair it, or delete it to reclaim the space.
          </p>
        )}

        {downloading && (
          <div className="mt-3">
            <div className="h-2 w-full overflow-hidden rounded-full bg-ink/10">
              <div
                className="h-full bg-brand transition-all"
                style={{ width: `${downloadProgress(model.status)}%` }}
              />
            </div>
            <p className="mt-1 text-xs text-ink-muted">
              Downloading… {downloadProgress(model.status)}%
            </p>
          </div>
        )}
      </div>
    );
  };

  if (loading) {
    return <div className="text-sm text-ink-muted">Loading models…</div>;
  }

  // One list, every model, in whatever order the sort control asks for. The old
  // split — recommended above a collapsed "All models" — hid the catalog behind a
  // click and made a sort apply to two lists separately. What it communicated
  // survives as the "Recommended" pill on the card.
  const q = query.trim().toLowerCase();
  // On disk, not merely usable: a corrupted or half-downloaded model is still
  // installed, and hiding the row someone needs to delete or retry is the one
  // outcome this filter must not produce.
  const installedCount = models.filter((m) => m.status !== 'Missing').length;
  const listed = sortModels(models, sort)
    .filter((m) => !installedOnly || m.status !== 'Missing')
    .filter(
    (m) =>
      !q ||
      [
        getModelLabel(m.name),
        m.description,
        m.accuracy,
        m.speed,
        m.diarizes ? 'labels speakers diarization' : '',
        languageHaystack(m.languages),
      ]
        .join(' ')
        .toLowerCase()
        .includes(q)
    );

  return (
    <div className="space-y-3">
      {error && (
        <div className="rounded-md bg-danger-soft p-3 text-sm text-danger-ink">{error}</div>
      )}

      <div className="flex flex-wrap items-start justify-between gap-3">
        <p className="max-w-[52ch] text-xs text-ink-muted">
          Quality is a tier from each model&apos;s measured error rate — the WER beside
          it is that measurement. Speed is estimated from file size. A WER only ranks
          against models measured on the same set; hover it to see which.
        </p>
        <div className="flex shrink-0 flex-wrap items-center gap-2">
          <div className="relative">
            <Search
              aria-hidden
              className="pointer-events-none absolute left-2.5 top-1/2 h-4 w-4 -translate-y-1/2 text-ink-faint"
            />
            <input
              type="search"
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="Search models"
              aria-label="Search transcription models"
              className={cn(
                'h-8 w-48 rounded-md border border-line bg-sunken pl-8 pr-2 text-xs text-ink',
                'placeholder:text-ink-muted',
                'transition-colors duration-fast',
                'hover:border-line-strong focus:border-brand focus:bg-elevated'
              )}
            />
          </div>
          <label className="flex cursor-pointer items-center gap-2 text-sm text-ink-muted">
            <Switch
              checked={installedOnly}
              onCheckedChange={setInstalledOnly}
              aria-label="Show only installed models"
            />
            Installed only
            <span className="readout text-2xs text-ink-faint">({installedCount})</span>
          </label>
          <span className="text-sm text-ink-muted">Sort by</span>
          <Select value={sort} onValueChange={(v) => setSort(v as ModelSort)}>
            <SelectTrigger className="h-8 w-40" aria-label="Sort models by">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {Object.entries(MODEL_SORT_LABELS).map(([value, label]) => (
                <SelectItem key={value} value={value}>
                  {label}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>
      </div>

      {listed.length === 0 && (q || installedOnly) ? (
        <p className="py-6 text-center text-sm text-ink-muted">
          {installedOnly && installedCount === 0
            ? 'No models are installed yet. Turn off “Installed only” to download one.'
            : `No models match “${query.trim()}”.`}
        </p>
      ) : (
        listed.map(card)
      )}

      <button
        type="button"
        className="text-xs text-ink-muted underline"
        onClick={() => TranscribeAPI.openModelsFolder()}
      >
        Open models folder
      </button>
    </div>
  );
}
