export type JobState =
  | 'queued'
  | 'downloading'
  | 'paused'
  | 'completed'
  | 'failed'
  | 'cancelled';

export interface JobOptions {
  output_dir?: string;
  filename?: string;
  connections?: number;
  max_bytes_per_sec?: number;
  headers?: [string, string][];
  checksum?: string;
  overwrite?: boolean;
  resume?: boolean;
  allow_private_ips?: boolean;
}

export interface JobInfo {
  id: string;
  url: string;
  dest: string;
  status: JobState;
  downloaded_bytes: number;
  total_bytes: number | null;
  error: string | null;
  created_at: number;
  updated_at: number;
}

export interface DaemonInfo {
  version: string;
  protocol_version: number;
  pid: number;
  uptime_secs: number;
  active_jobs: number;
  queued_jobs: number;
  total_jobs: number;
}

export interface JobProgressPayload {
  id: string;
  downloaded: number;
  total: number | null;
  speed_bps: number;
  state: JobState;
}

export interface UIJob extends JobInfo {
  speed_bps: number;
  eta_secs: number | null;
  category: string;
  filename: string;
  selected?: boolean;
}

export type CategoryFilter =
  | 'all'
  | 'downloading'
  | 'completed'
  | 'paused'
  | 'failed'
  | 'videos'
  | 'torrents'
  | 'documents'
  | 'archives';
