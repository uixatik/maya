/**
 * Maya Downloader - Popup Script
 */

document.addEventListener('DOMContentLoaded', async () => {
  const statusBadge = document.getElementById('statusBadge');
  const statusText = document.getElementById('statusText');
  const urlInput = document.getElementById('urlInput');
  const filenameInput = document.getElementById('filenameInput');
  const connectionsSelect = document.getElementById('connectionsSelect');
  const pasteBtn = document.getElementById('pasteBtn');
  const downloadBtn = document.getElementById('downloadBtn');
  const feedbackMsg = document.getElementById('feedbackMessage');
  const interceptToggle = document.getElementById('interceptToggle');
  const mediaToggle = document.getElementById('mediaToggle');
  const mediaSection = document.getElementById('mediaSection');
  const mediaList = document.getElementById('mediaList');
  const mediaCountBadge = document.getElementById('mediaCountBadge');
  const openWebUi = document.getElementById('openWebUi');

  function showFeedback(text, type = 'info', duration = 3000) {
    feedbackMsg.textContent = text;
    feedbackMsg.className = `feedback-msg ${type}`;
    if (duration > 0) {
      setTimeout(() => {
        feedbackMsg.textContent = '';
        feedbackMsg.className = 'feedback-msg';
      }, duration);
    }
  }

  // 1. Check Native Host & Daemon status
  async function checkHostStatus() {
    statusBadge.className = 'status-badge checking';
    statusText.textContent = 'Connecting...';

    chrome.runtime.sendMessage(
      { action: 'native_dispatch', payload: { action: 'ping' } },
      (res) => {
        if (!res || !res.success) {
          statusBadge.className = 'status-badge disconnected';
          statusText.textContent = 'Host Offline';
          return;
        }

        const data = res.data || {};
        const daemonInfo = data.daemon || {};
        if (daemonInfo.daemon_connected) {
          statusBadge.className = 'status-badge connected';
          statusText.textContent = 'Daemon Ready';
        } else {
          statusBadge.className = 'status-badge warning';
          statusText.textContent = 'Host Active (Mayad Off)';
        }
      }
    );
  }

  // 2. Load stored preferences
  chrome.runtime.sendMessage({ action: 'get_settings' }, (settings) => {
    if (settings) {
      if (typeof settings.interceptDownloads === 'boolean') {
        interceptToggle.checked = settings.interceptDownloads;
      }
      if (typeof settings.captureMediaStreams === 'boolean') {
        mediaToggle.checked = settings.captureMediaStreams;
      }
    }
  });

  // Toggle listeners
  interceptToggle.addEventListener('change', () => {
    chrome.runtime.sendMessage({
      action: 'save_settings',
      settings: { interceptDownloads: interceptToggle.checked }
    });
  });

  mediaToggle.addEventListener('change', () => {
    chrome.runtime.sendMessage({
      action: 'save_settings',
      settings: { captureMediaStreams: mediaToggle.checked }
    });
  });

  // 3. Inspect active tab for pre-filling or detected streams
  chrome.tabs.query({ active: true, currentWindow: true }, (tabs) => {
    if (!tabs || tabs.length === 0) return;
    const tab = tabs[0];
    const tabUrl = tab.url || '';

    // If active tab is on YouTube, suggest it in the URL box
    if (tabUrl.includes('youtube.com/watch') || tabUrl.includes('youtu.be/')) {
      urlInput.value = tabUrl;
    }

    // Query detected media streams on active tab
    chrome.runtime.sendMessage({ action: 'get_tab_media', tabId: tab.id }, (res) => {
      if (res && res.media && res.media.length > 0) {
        mediaSection.style.display = 'flex';
        mediaCountBadge.textContent = `${res.media.length}`;
        mediaList.innerHTML = '';

        res.media.forEach((item) => {
          const row = document.createElement('div');
          row.className = 'media-item';

          const infoDiv = document.createElement('div');
          infoDiv.className = 'media-info';

          const title = document.createElement('span');
          title.className = 'media-title';
          title.textContent = item.title || item.url.split('/').pop().split('?')[0] || 'Media Stream';
          title.title = item.url;

          const type = document.createElement('span');
          type.className = 'media-type';
          type.textContent = item.type ? item.type.toUpperCase() : 'STREAM';

          infoDiv.appendChild(title);
          infoDiv.appendChild(type);

          const grabBtn = document.createElement('button');
          grabBtn.className = 'btn btn-secondary btn-sm';
          grabBtn.textContent = 'Capture';
          grabBtn.addEventListener('click', () => {
            urlInput.value = item.url;
            downloadBtn.click();
          });

          row.appendChild(infoDiv);
          row.appendChild(grabBtn);
          mediaList.appendChild(row);
        });
      }
    });
  });

  // 4. Paste button
  pasteBtn.addEventListener('click', async () => {
    try {
      const text = await navigator.clipboard.readText();
      if (text) {
        urlInput.value = text.trim();
        showFeedback('URL pasted from clipboard', 'info', 1500);
      }
    } catch (e) {
      showFeedback('Clipboard permission denied', 'error');
    }
  });

  // 5. Download button click
  downloadBtn.addEventListener('click', () => {
    const rawUrl = urlInput.value.trim();
    if (!rawUrl) {
      showFeedback('Please enter or paste a valid URL', 'error');
      urlInput.focus();
      return;
    }

    const filename = filenameInput.value.trim() || undefined;
    const connections = parseInt(connectionsSelect.value, 10) || 8;

    showFeedback('Dispatching to Maya...', 'info', 0);
    downloadBtn.disabled = true;

    chrome.runtime.sendMessage(
      {
        action: 'native_dispatch',
        payload: {
          action: 'download',
          url: rawUrl,
          filename: filename,
          connections: connections
        }
      },
      (res) => {
        downloadBtn.disabled = false;
        if (res && res.success) {
          showFeedback('Download added to Maya queue!', 'success', 3000);
          urlInput.value = '';
          filenameInput.value = '';
        } else {
          const err = res && res.error ? res.error : 'Unknown error dispatching download';
          showFeedback(`Error: ${err}`, 'error', 5000);
        }
      }
    );
  });

  // 6. Quick Links
  openWebUi.addEventListener('click', (e) => {
    e.preventDefault();
    chrome.tabs.create({ url: 'http://localhost:7860' });
  });

  // Initial check
  checkHostStatus();
});
