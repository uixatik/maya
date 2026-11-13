import {
  addJob,
  cancelJob,
  getDaemonStatus,
  listJobs,
  onJobProgress,
  pauseJob,
  resumeJob,
} from './api';
import type { CategoryFilter, JobInfo, JobOptions, JobProgressPayload, UIJob } from './types';

// State
let jobs: Map<string, UIJob> = new Map();
let currentFilter: CategoryFilter = 'all';
let searchQuery = '';
let isDaemonOnline = false;

// DOM Elements
const jobsTableBody = document.getElementById('jobs-table-body') as HTMLTableSectionElement;
const emptyState = document.getElementById('empty-state') as HTMLDivElement;
const filterInput = document.getElementById('filter-input') as HTMLInputElement;
const selectAllCheckbox = document.getElementById('select-all') as HTMLInputElement;

// Toolbar & Buttons
const btnAddModal = document.getElementById('btn-add-modal') as HTMLButtonElement;
const btnResumeSelected = document.getElementById('btn-resume-selected') as HTMLButtonElement;
const btnPauseSelected = document.getElementById('btn-pause-selected') as HTMLButtonElement;
const btnDeleteSelected = document.getElementById('btn-delete-selected') as HTMLButtonElement;
const btnSettingsModal = document.getElementById('btn-settings-modal') as HTMLButtonElement;

// Speed & Status indicators
const globalSpeedDown = document.getElementById('global-speed-down') as HTMLElement;
const globalSpeedUp = document.getElementById('global-speed-up') as HTMLElement;
const daemonDot = document.getElementById('daemon-dot') as HTMLElement;
const daemonStatusText = document.getElementById('daemon-status-text') as HTMLElement;
const threadsIndicator = document.getElementById('threads-indicator') as HTMLElement;
const totalRatesIndicator = document.getElementById('total-rates-indicator') as HTMLElement;
const jobsSummaryIndicator = document.getElementById('jobs-summary-indicator') as HTMLElement;
const viewStatusText = document.getElementById('view-status-text') as HTMLElement;

// Category counter elements
const countElements: Record<string, HTMLElement | null> = {
  all: document.getElementById('count-all'),
  downloading: document.getElementById('count-downloading'),
  completed: document.getElementById('count-completed'),
  paused: document.getElementById('count-paused'),
  failed: document.getElementById('count-failed'),
  videos: document.getElementById('count-videos'),
  torrents: document.getElementById('count-torrents'),
  archives: document.getElementById('count-archives'),
  documents: document.getElementById('count-documents'),
};

// Add Modal elements
const addModal = document.getElementById('add-modal') as HTMLDivElement;
const btnCloseAddModal = document.getElementById('btn-close-add-modal') as HTMLButtonElement;
const btnCancelAdd = document.getElementById('btn-cancel-add') as HTMLButtonElement;
const btnSubmitAdd = document.getElementById('btn-submit-add') as HTMLButtonElement;
const addUrl = document.getElementById('add-url') as HTMLInputElement;
const addCategory = document.getElementById('add-category') as HTMLSelectElement;
const addFilename = document.getElementById('add-filename') as HTMLInputElement;
const addDest = document.getElementById('add-dest') as HTMLInputElement;
const addConnections = document.getElementById('add-connections') as HTMLInputElement;
const sliderConnsVal = document.getElementById('slider-conns-val') as HTMLElement;

// Settings Modal elements
const settingsModal = document.getElementById('settings-modal') as HTMLDivElement;
const btnCloseSettingsModal = document.getElementById('btn-close-settings-modal') as HTMLButtonElement;
const btnSaveSettings = document.getElementById('btn-save-settings') as HTMLButtonElement;

// Helpers
function formatBytes(bytes: number | null | undefined): string {
  if (bytes == null || isNaN(bytes) || bytes === 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const k = 1024;
  const i = Math.floor(Math.log(bytes) / Math.log(k));
  const val = bytes / Math.pow(k, i);
  return `${val.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

function formatSpeed(bps: number | null | undefined): string {
  if (!bps || bps <= 0) return '0.0 KB/s';
  if (bps >= 1024 * 1024) {
    return `${(bps / (1024 * 1024)).toFixed(1)} MB/s`;
  }
  return `${(bps / 1024).toFixed(0)} KB/s`;
}

function formatEta(secs: number | null): string {
  if (secs == null || !isFinite(secs) || secs < 0) return '--:--';
  const h = Math.floor(secs / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = Math.floor(secs % 60);
  if (h > 0) {
    return `${h}h ${m.toString().padStart(2, '0')}m`;
  }
  return `${m.toString().padStart(2, '0')}:${s.toString().padStart(2, '0')}`;
}

function extractFilename(url: string, dest?: string): string {
  if (dest) {
    const parts = dest.split(/[/\\]/);
    const last = parts.pop();
    if (last && last.trim().length > 0) return last;
  }
  try {
    const parsed = new URL(url);
    const pathname = parsed.pathname;
    const parts = pathname.split('/');
    const last = parts.pop();
    if (last && last.trim().length > 0) return decodeURIComponent(last);
  } catch {
    // If not standard URL or magnet
    if (url.startsWith('magnet:')) {
      const match = url.match(/dn=([^&]+)/);
      if (match && match[1]) return decodeURIComponent(match[1]);
      return 'magnet-download.torrent';
    }
  }
  return 'download.bin';
}

function detectCategory(filename: string, url: string): string {
  const lower = (filename + ' ' + url).toLowerCase();
  if (lower.startsWith('magnet:') || lower.endsWith('.torrent')) return 'torrents';
  if (lower.match(/\.(mp4|mkv|webm|avi|mov|flv|wmv|m4v)$/) || lower.includes('youtube.com') || lower.includes('youtu.be')) return 'videos';
  if (lower.match(/\.(zip|tar|gz|bz2|xz|7z|rar|iso|zst)$/)) return 'archives';
  if (lower.match(/\.(pdf|epub|mobi|docx?|xlsx?|pptx?|txt|md)$/)) return 'documents';
  return 'general';
}

function getFileIconSvg(category: string): string {
  switch (category) {
    case 'videos':
      return `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polygon points="23 7 16 12 23 17 23 7"></polygon><rect x="1" y="5" width="15" height="14" rx="2" ry="2"></rect></svg>`;
    case 'torrents':
      return `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="10"></circle><path d="M12 8v8M8 12h8"></path></svg>`;
    case 'archives':
      return `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M21 8v13H3V8"></path><path d="M1 3h22v5H1z"></path><line x1="10" y1="12" x2="14" y2="12"></line></svg>`;
    case 'documents':
      return `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"></path><polyline points="14 2 14 8 20 8"></polyline></svg>`;
    default:
      return `<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"></path><polyline points="7 10 12 15 17 10"></polyline><line x1="12" y1="15" x2="12" y2="3"></line></svg>`;
  }
}

// Convert JobInfo to UIJob
function toUIJob(job: JobInfo, existing?: UIJob): UIJob {
  const filename = extractFilename(job.url, job.dest);
  const category = detectCategory(filename, job.url);
  const speed_bps = existing?.speed_bps ?? 0;
  let eta_secs: number | null = null;
  if (job.total_bytes && job.downloaded_bytes < job.total_bytes && speed_bps > 0) {
    eta_secs = Math.round((job.total_bytes - job.downloaded_bytes) / speed_bps);
  }

  return {
    ...job,
    filename,
    category,
    speed_bps,
    eta_secs,
    selected: existing?.selected ?? false,
  };
}

// Render functions
function updateCounters(): void {
  const counts: Record<string, number> = {
    all: jobs.size,
    downloading: 0,
    completed: 0,
    paused: 0,
    failed: 0,
    videos: 0,
    torrents: 0,
    archives: 0,
    documents: 0,
  };

  for (const job of jobs.values()) {
    if (job.status in counts) {
      counts[job.status] += 1;
    }
    if (job.category in counts) {
      counts[job.category] += 1;
    }
  }

  for (const [key, el] of Object.entries(countElements)) {
    if (el) {
      el.textContent = (counts[key] || 0).toString();
    }
  }

  // Update Aggregate speeds
  let totalBps = 0;
  let activeCount = 0;
  for (const job of jobs.values()) {
    if (job.status === 'downloading') {
      totalBps += job.speed_bps;
      activeCount += 1;
    }
  }

  const speedText = formatSpeed(totalBps);
  globalSpeedDown.textContent = speedText;
  totalRatesIndicator.textContent = `↓ ${speedText} | ↑ 0.0 KB/s`;
  threadsIndicator.textContent = `⚡ ${activeCount * 8} threads active`;
  jobsSummaryIndicator.textContent = `${activeCount} active / ${jobs.size} total`;
}

function matchesFilter(job: UIJob): boolean {
  if (currentFilter === 'all') {
    // Show all
  } else if (['downloading', 'completed', 'paused', 'failed'].includes(currentFilter)) {
    if (job.status !== currentFilter) return false;
  } else {
    if (job.category !== currentFilter) return false;
  }

  if (searchQuery.trim().length > 0) {
    const q = searchQuery.toLowerCase();
    return job.filename.toLowerCase().includes(q) || job.url.toLowerCase().includes(q);
  }

  return true;
}

function renderTable(): void {
  jobsTableBody.innerHTML = '';
  const visibleJobs = Array.from(jobs.values()).filter(matchesFilter);

  if (visibleJobs.length === 0) {
    emptyState.style.display = 'flex';
  } else {
    emptyState.style.display = 'none';
  }

  for (const job of visibleJobs) {
    const tr = document.createElement('tr');
    tr.className = `job-row ${job.selected ? 'selected' : ''}`;
    tr.dataset.jobId = job.id;

    const percent =
      job.total_bytes && job.total_bytes > 0
        ? Math.min(100, (job.downloaded_bytes / job.total_bytes) * 100)
        : job.status === 'completed'
        ? 100
        : 0;

    const progressClass =
      job.status === 'paused'
        ? 'paused'
        : job.status === 'failed'
        ? 'failed'
        : job.status === 'completed'
        ? 'completed'
        : '';

    tr.innerHTML = `
      <td>
        <input type="checkbox" class="job-checkbox" ${job.selected ? 'checked' : ''} />
      </td>
      <td class="col-name">
        <div class="file-title-wrap">
          <div class="file-type-icon">${getFileIconSvg(job.category)}</div>
          <div class="file-info">
            <span class="file-name" title="${job.filename}">${job.filename}</span>
            <span class="file-url" title="${job.url}">${job.url}</span>
          </div>
        </div>
      </td>
      <td>${formatBytes(job.total_bytes || job.downloaded_bytes)}</td>
      <td class="progress-col">
        <div class="progress-wrap">
          <div class="progress-bar-bg">
            <div class="progress-bar-fill ${progressClass}" style="width: ${percent.toFixed(1)}%;"></div>
          </div>
          <div class="progress-labels">
            <span>${percent.toFixed(1)}%</span>
            <span>${formatBytes(job.downloaded_bytes)}</span>
          </div>
        </div>
      </td>
      <td>${job.status === 'downloading' ? formatSpeed(job.speed_bps) : '-'}</td>
      <td>${job.status === 'downloading' ? formatEta(job.eta_secs) : '-'}</td>
      <td>
        <span class="status-badge ${job.status}">${job.status}</span>
      </td>
      <td>
        <div class="row-actions">
          ${
            job.status === 'downloading'
              ? `<button class="btn-tiny btn-row-pause" title="Pause">⏸</button>`
              : `<button class="btn-tiny btn-row-resume" title="Resume">▶</button>`
          }
          <button class="btn-tiny btn-row-cancel" title="Cancel/Delete">✕</button>
        </div>
      </td>
    `;

    // Row selection listener
    const checkbox = tr.querySelector('.job-checkbox') as HTMLInputElement;
    checkbox.addEventListener('change', (e) => {
      e.stopPropagation();
      job.selected = checkbox.checked;
      tr.classList.toggle('selected', job.selected);
      updateSelectionButtons();
    });

    tr.addEventListener('click', (e) => {
      const target = e.target as HTMLElement;
      if (target.tagName.toLowerCase() === 'button' || target.tagName.toLowerCase() === 'input') {
        return;
      }
      job.selected = !job.selected;
      checkbox.checked = job.selected;
      tr.classList.toggle('selected', job.selected);
      updateSelectionButtons();
    });

    // Row action buttons
    const btnPause = tr.querySelector('.btn-row-pause');
    if (btnPause) {
      btnPause.addEventListener('click', async (e) => {
        e.stopPropagation();
        await handlePauseJob(job.id);
      });
    }

    const btnResume = tr.querySelector('.btn-row-resume');
    if (btnResume) {
      btnResume.addEventListener('click', async (e) => {
        e.stopPropagation();
        await handleResumeJob(job.id);
      });
    }

    const btnCancel = tr.querySelector('.btn-row-cancel');
    if (btnCancel) {
      btnCancel.addEventListener('click', async (e) => {
        e.stopPropagation();
        await handleCancelJob(job.id);
      });
    }

    jobsTableBody.appendChild(tr);
  }

  updateCounters();
  updateSelectionButtons();
}

function updateSelectionButtons(): void {
  const selectedCount = Array.from(jobs.values()).filter((j) => j.selected).length;
  btnResumeSelected.disabled = selectedCount === 0;
  btnPauseSelected.disabled = selectedCount === 0;
  btnDeleteSelected.disabled = selectedCount === 0;
}

// Actions
async function handlePauseJob(id: string): Promise<void> {
  try {
    const updated = await pauseJob(id);
    const existing = jobs.get(id);
    jobs.set(id, toUIJob(updated, existing));
    renderTable();
  } catch (err) {
    console.error('Failed to pause job:', err);
  }
}

async function handleResumeJob(id: string): Promise<void> {
  try {
    const updated = await resumeJob(id);
    const existing = jobs.get(id);
    jobs.set(id, toUIJob(updated, existing));
    renderTable();
  } catch (err) {
    console.error('Failed to resume job:', err);
  }
}

async function handleCancelJob(id: string): Promise<void> {
  try {
    await cancelJob(id);
    jobs.delete(id);
    renderTable();
  } catch (err) {
    console.error('Failed to cancel job:', err);
  }
}

async function refreshJobs(): Promise<void> {
  try {
    const list = await listJobs();
    for (const item of list) {
      const existing = jobs.get(item.id);
      jobs.set(item.id, toUIJob(item, existing));
    }
    renderTable();
  } catch (err) {
    console.error('Failed to refresh jobs:', err);
  }
}

async function refreshDaemon(): Promise<void> {
  try {
    const status = await getDaemonStatus();
    isDaemonOnline = true;
    daemonDot.className = 'status-dot online';
    daemonStatusText.textContent = `Daemon Online (PID: ${status.pid})`;
  } catch {
    isDaemonOnline = false;
    daemonDot.className = 'status-dot offline';
    daemonStatusText.textContent = 'Daemon Offline';
  }
}

function handleProgressEvent(payload: JobProgressPayload): void {
  const existing = jobs.get(payload.id);
  if (existing) {
    existing.downloaded_bytes = payload.downloaded;
    if (payload.total != null) existing.total_bytes = payload.total;
    existing.speed_bps = payload.speed_bps;
    existing.status = payload.state;
    if (existing.total_bytes && existing.speed_bps > 0) {
      existing.eta_secs = Math.round((existing.total_bytes - existing.downloaded_bytes) / existing.speed_bps);
    } else {
      existing.eta_secs = null;
    }
    renderTable();
  }
}

// Event Listeners Initialization
function initEventListeners(): void {
  // Category item clicks
  document.querySelectorAll('.category-item').forEach((el) => {
    el.addEventListener('click', () => {
      document.querySelectorAll('.category-item').forEach((c) => c.classList.remove('active'));
      el.classList.add('active');
      currentFilter = (el.getAttribute('data-category') || 'all') as CategoryFilter;
      viewStatusText.textContent = `Showing ${currentFilter} downloads`;
      renderTable();
    });
  });

  // Search filter
  filterInput.addEventListener('input', () => {
    searchQuery = filterInput.value;
    renderTable();
  });

  // Select all checkbox
  selectAllCheckbox.addEventListener('change', () => {
    const checked = selectAllCheckbox.checked;
    for (const job of jobs.values()) {
      if (matchesFilter(job)) {
        job.selected = checked;
      }
    }
    renderTable();
  });

  // Add Modal open/close
  btnAddModal.addEventListener('click', () => {
    addModal.classList.add('open');
    addUrl.focus();
  });

  const closeAdd = () => addModal.classList.remove('open');
  btnCloseAddModal.addEventListener('click', closeAdd);
  btnCancelAdd.addEventListener('click', closeAdd);

  // Settings Modal open/close
  btnSettingsModal.addEventListener('click', () => settingsModal.classList.add('open'));
  const closeSettings = () => settingsModal.classList.remove('open');
  btnCloseSettingsModal.addEventListener('click', closeSettings);
  btnSaveSettings.addEventListener('click', closeSettings);

  // Add Connections slider
  addConnections.addEventListener('input', () => {
    sliderConnsVal.textContent = `${addConnections.value} connections`;
  });

  // Submit Add Download
  btnSubmitAdd.addEventListener('click', async () => {
    const url = addUrl.value.trim();
    if (!url) return;

    const connections = parseInt(addConnections.value, 10) || 8;
    const filename = addFilename.value.trim() || undefined;
    const outputDir = addDest.value.trim() || undefined;

    const options: JobOptions = {
      connections,
      filename,
      output_dir: outputDir,
    };

    try {
      const newJob = await addJob(url, options);
      jobs.set(newJob.id, toUIJob(newJob));
      addUrl.value = '';
      addFilename.value = '';
      closeAdd();
      renderTable();
    } catch (err) {
      alert(`Error adding download: ${err}`);
    }
  });

  // Bulk actions
  btnPauseSelected.addEventListener('click', async () => {
    for (const job of jobs.values()) {
      if (job.selected && job.status === 'downloading') {
        await handlePauseJob(job.id);
      }
    }
  });

  btnResumeSelected.addEventListener('click', async () => {
    for (const job of jobs.values()) {
      if (job.selected && (job.status === 'paused' || job.status === 'queued')) {
        await handleResumeJob(job.id);
      }
    }
  });

  btnDeleteSelected.addEventListener('click', async () => {
    const selected = Array.from(jobs.values()).filter((j) => j.selected);
    for (const job of selected) {
      await handleCancelJob(job.id);
    }
  });
}

// App Bootstrap
async function startApp(): Promise<void> {
  initEventListeners();
  await refreshDaemon();
  await refreshJobs();

  // Listen to Tauri progress events
  try {
    await onJobProgress(handleProgressEvent);
  } catch (err) {
    console.error('Failed to subscribe to progress events:', err);
  }

  // Periodic polling for status & job updates
  setInterval(async () => {
    await refreshDaemon();
    await refreshJobs();
  }, 3000);
}

startApp().catch(console.error);
