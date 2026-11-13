import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type { DaemonInfo, JobInfo, JobOptions, JobProgressPayload } from './types';

// Detect if running inside Tauri WebView
export const isTauri = (): boolean => {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
};

// In-memory fallback mock for web preview/development
const mockJobs: JobInfo[] = [
  {
    id: 'demo-job-1',
    url: 'https://ash-speed.hetzner.com/100MB.bin',
    dest: 'C:\\Users\\Downloads\\100MB.bin',
    status: 'downloading',
    downloaded_bytes: 48_234_496,
    total_bytes: 104_857_600,
    error: null,
    created_at: Date.now() - 30_000,
    updated_at: Date.now(),
  },
  {
    id: 'demo-job-2',
    url: 'https://releases.ubuntu.com/24.04/ubuntu-24.04-desktop-amd64.iso',
    dest: 'C:\\Users\\Downloads\\ubuntu-24.04.iso',
    status: 'paused',
    downloaded_bytes: 1_234_567_890,
    total_bytes: 6_123_456_789,
    error: null,
    created_at: Date.now() - 3_600_000,
    updated_at: Date.now() - 100_000,
  },
  {
    id: 'demo-job-3',
    url: 'https://example.com/videos/tutorial.mp4',
    dest: 'C:\\Users\\Downloads\\tutorial.mp4',
    status: 'completed',
    downloaded_bytes: 154_892_100,
    total_bytes: 154_892_100,
    error: null,
    created_at: Date.now() - 86_400_000,
    updated_at: Date.now() - 86_000_000,
  },
];

export async function addJob(url: string, options: JobOptions = {}): Promise<JobInfo> {
  if (isTauri()) {
    return await invoke<JobInfo>('add_job', { url, options });
  }

  const filename = options.filename || url.split('/').pop()?.split('?')[0] || 'download.bin';
  const newJob: JobInfo = {
    id: `mock-${Date.now()}`,
    url,
    dest: `C:\\Users\\Downloads\\${filename}`,
    status: 'downloading',
    downloaded_bytes: 0,
    total_bytes: 52_428_800,
    error: null,
    created_at: Date.now(),
    updated_at: Date.now(),
  };
  mockJobs.unshift(newJob);
  return newJob;
}

export async function listJobs(): Promise<JobInfo[]> {
  if (isTauri()) {
    return await invoke<JobInfo[]>('list_jobs');
  }
  return [...mockJobs];
}

export async function pauseJob(id: string): Promise<JobInfo> {
  if (isTauri()) {
    return await invoke<JobInfo>('pause_job', { id });
  }
  const job = mockJobs.find((j) => j.id === id);
  if (job) {
    job.status = 'paused';
    return job;
  }
  throw new Error(`Job not found: ${id}`);
}

export async function resumeJob(id: string): Promise<JobInfo> {
  if (isTauri()) {
    return await invoke<JobInfo>('resume_job', { id });
  }
  const job = mockJobs.find((j) => j.id === id);
  if (job) {
    job.status = 'downloading';
    return job;
  }
  throw new Error(`Job not found: ${id}`);
}

export async function cancelJob(id: string): Promise<JobInfo> {
  if (isTauri()) {
    return await invoke<JobInfo>('cancel_job', { id });
  }
  const index = mockJobs.findIndex((j) => j.id === id);
  if (index !== -1) {
    const [job] = mockJobs.splice(index, 1);
    job.status = 'cancelled';
    return job;
  }
  throw new Error(`Job not found: ${id}`);
}

export async function getDaemonStatus(): Promise<DaemonInfo> {
  if (isTauri()) {
    return await invoke<DaemonInfo>('get_daemon_status');
  }
  return {
    version: '0.0.1 (Web Preview)',
    protocol_version: 1,
    pid: 1234,
    uptime_secs: 1420,
    active_jobs: mockJobs.filter((j) => j.status === 'downloading').length,
    queued_jobs: mockJobs.filter((j) => j.status === 'queued').length,
    total_jobs: mockJobs.length,
  };
}

export async function onJobProgress(
  callback: (payload: JobProgressPayload) => void
): Promise<UnlistenFn> {
  if (isTauri()) {
    return await listen<JobProgressPayload>('download-progress', (event) => {
      callback(event.payload);
    });
  }

  // Simulated progress events for web preview
  const interval = setInterval(() => {
    const downloading = mockJobs.filter((j) => j.status === 'downloading');
    for (const job of downloading) {
      if (job.total_bytes && job.downloaded_bytes < job.total_bytes) {
        const chunk = Math.min(1024 * 512, job.total_bytes - job.downloaded_bytes);
        job.downloaded_bytes += chunk;
        if (job.downloaded_bytes >= job.total_bytes) {
          job.status = 'completed';
        }
        callback({
          id: job.id,
          downloaded: job.downloaded_bytes,
          total: job.total_bytes,
          speed_bps: 1024 * 1024 * 4.5,
          state: job.status,
        });
      }
    }
  }, 1000);

  return () => clearInterval(interval);
}
