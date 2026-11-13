/**
 * Maya Downloader - Background Service Worker
 * Intercepts downloads, registers context menus, detects YouTube & streaming media,
 * and communicates with the Maya Native Messaging Host (`com.maya.downloader`).
 */

const NATIVE_HOST = 'com.maya.downloader';

// Default user settings
const DEFAULT_SETTINGS = {
  interceptDownloads: true,
  captureMediaStreams: true,
  minSizeMb: 0,
  interceptExtensions: [
    'zip', 'rar', '7z', 'tar', 'gz', 'bz2', 'xz', 'iso', 'bin', 'cue',
    'exe', 'msi', 'dmg', 'pkg', 'deb', 'rpm', 'apk', 'appimage',
    'mp4', 'mkv', 'avi', 'mov', 'flv', 'webm', 'ts', 'm4v', '3gp',
    'mp3', 'flac', 'wav', 'aac', 'ogg', 'm4a', 'wma', 'opus',
    'pdf', 'epub', 'torrent'
  ]
};

// Internal tracking to prevent recursive download loops
const mayaManagedDownloadIds = new Set();

// In-memory per-tab cache of detected media streams
const tabMediaMap = new Map(); // tabId -> Array<{ url, type, mimeType, title, timestamp }>

/**
 * Send a message to the Maya Native Messaging Host.
 * @param {object} message
 * @returns {Promise<object>}
 */
function sendNativeMessage(message) {
  return new Promise((resolve) => {
    try {
      chrome.runtime.sendNativeMessage(NATIVE_HOST, message, (response) => {
        if (chrome.runtime.lastError) {
          const err = chrome.runtime.lastError.message;
          console.warn('[Maya NMH Error]:', err);
          resolve({
            status: 'error',
            success: false,
            error: `Native Messaging error: ${err}`
          });
        } else {
          resolve(response || { status: 'ok', success: true });
        }
      });
    } catch (e) {
      console.error('[Maya Native Message Exception]:', e);
      resolve({
        status: 'error',
        success: false,
        error: e.message || 'Failed to dispatch native message'
      });
    }
  });
}

/**
 * Show a quick visual badge on the extension action icon.
 * @param {number} tabId
 * @param {string} text
 * @param {string} color
 */
function setBadge(tabId, text, color = '#7c3aed') {
  if (!chrome.action) return;
  chrome.action.setBadgeText({ text, tabId });
  chrome.action.setBadgeBackgroundColor({ color, tabId });
}

// ---------------------------------------------------------------------------
// 1. Context Menus setup
// ---------------------------------------------------------------------------
chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: 'maya-download-link',
      title: 'Download with Maya',
      contexts: ['link']
    });

    chrome.contextMenus.create({
      id: 'maya-download-media',
      title: 'Download Media with Maya',
      contexts: ['video', 'audio', 'image']
    });

    chrome.contextMenus.create({
      id: 'maya-download-selection',
      title: 'Download URL "%s" with Maya',
      contexts: ['selection']
    });

    chrome.contextMenus.create({
      id: 'maya-download-page',
      title: 'Download Current Page with Maya',
      contexts: ['page']
    });
  });

  // Initialize default settings if not already present
  chrome.storage.local.get(DEFAULT_SETTINGS, (items) => {
    chrome.storage.local.set(items);
  });
});

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  let targetUrl = null;

  if (info.menuItemId === 'maya-download-link' && info.linkUrl) {
    targetUrl = info.linkUrl;
  } else if (info.menuItemId === 'maya-download-media' && info.srcUrl) {
    targetUrl = info.srcUrl;
  } else if (info.menuItemId === 'maya-download-selection' && info.selectionText) {
    const text = info.selectionText.trim();
    if (text.startsWith('http://') || text.startsWith('https://') || text.startsWith('magnet:')) {
      targetUrl = text;
    }
  } else if (info.menuItemId === 'maya-download-page' && info.pageUrl) {
    targetUrl = info.pageUrl;
  }

  if (targetUrl) {
    const tabUrl = tab && tab.url ? tab.url : targetUrl;
    const tabTitle = tab && tab.title ? tab.title : '';

    const res = await sendNativeMessage({
      action: 'download',
      url: targetUrl,
      referrer: tabUrl,
      title: tabTitle
    });

    if (tab && tab.id) {
      if (res.success) {
        setBadge(tab.id, '✓', '#10b981');
      } else {
        setBadge(tab.id, '!', '#ef4444');
      }
      setTimeout(() => setBadge(tab.id, ''), 2500);
    }
  }
});

// ---------------------------------------------------------------------------
// 2. Download Interception
// ---------------------------------------------------------------------------
chrome.downloads.onCreated.addListener(async (downloadItem) => {
  // Ignore downloads triggered directly by this extension
  if (mayaManagedDownloadIds.has(downloadItem.id)) {
    return;
  }

  // Get current interception setting
  const settings = await new Promise((res) => {
    chrome.storage.local.get(DEFAULT_SETTINGS, res);
  });

  if (!settings.interceptDownloads) {
    return;
  }

  const url = downloadItem.url || downloadItem.finalUrl;
  if (!url || !url.startsWith('http')) {
    return;
  }

  // Check file extension
  const filename = downloadItem.filename ? downloadItem.filename.split(/[\/\\]/).pop() : '';
  const ext = filename.split('.').pop().toLowerCase();

  const isMatchingExt = ext && settings.interceptExtensions.includes(ext);
  const isDirectDownload = downloadItem.fileSize > 0 || isMatchingExt;

  if (isDirectDownload) {
    mayaManagedDownloadIds.add(downloadItem.id);

    try {
      // Cancel browser download and remove from browser history shelf
      await chrome.downloads.cancel(downloadItem.id);
      await chrome.downloads.erase({ id: downloadItem.id });
    } catch (e) {
      console.warn('[Maya Intercept] Could not cancel/erase browser download:', e);
    }

    // Forward to Maya Native Messaging Host
    console.log('[Maya Intercept] Forwarding download to Maya:', url, filename);
    await sendNativeMessage({
      action: 'download',
      url: url,
      filename: filename || undefined,
      referrer: downloadItem.referrer || undefined
    });
  }
});

// ---------------------------------------------------------------------------
// 3. YouTube Watch URL and Media Stream Detection
// ---------------------------------------------------------------------------
function isYouTubeUrl(url) {
  if (!url) return false;
  try {
    const parsed = new URL(url);
    const host = parsed.hostname.toLowerCase();
    if (host.includes('youtube.com') || host === 'youtu.be') {
      return parsed.pathname.includes('/watch') ||
             parsed.pathname.includes('/shorts/') ||
             host === 'youtu.be';
    }
  } catch (e) {}
  return false;
}

// Track tab navigation to detect YouTube pages
chrome.tabs.onUpdated.addListener((tabId, changeInfo, tab) => {
  if (changeInfo.status === 'complete' && tab && tab.url) {
    if (isYouTubeUrl(tab.url)) {
      const streams = tabMediaMap.get(tabId) || [];
      const exists = streams.some((s) => s.url === tab.url);
      if (!exists) {
        streams.push({
          url: tab.url,
          title: tab.title || 'YouTube Media',
          type: 'youtube',
          mimeType: 'video/youtube',
          timestamp: Date.now()
        });
        tabMediaMap.set(tabId, streams);
      }
      setBadge(tabId, '▶', '#8b5cf6');
    }
  }
});

// Clean up closed tabs
chrome.tabs.onRemoved.addListener((tabId) => {
  tabMediaMap.delete(tabId);
});

// Inspect network responses for media streaming manifests or video chunks
if (chrome.webRequest && chrome.webRequest.onHeadersReceived) {
  chrome.webRequest.onHeadersReceived.addListener(
    (details) => {
      if (details.tabId <= 0 || !details.responseHeaders) return;

      const ctHeader = details.responseHeaders.find(
        (h) => h.name.toLowerCase() === 'content-type'
      );
      const contentType = ctHeader && ctHeader.value ? ctHeader.value.toLowerCase() : '';

      const isMedia =
        contentType.startsWith('video/') ||
        contentType.startsWith('audio/') ||
        contentType.includes('application/vnd.apple.mpegurl') || // HLS .m3u8
        contentType.includes('application/dash+xml') ||          // MPEG-DASH .mpd
        details.url.includes('.m3u8') ||
        details.url.includes('.mpd');

      if (isMedia) {
        const streams = tabMediaMap.get(details.tabId) || [];
        const exists = streams.some((s) => s.url === details.url);
        if (!exists && streams.length < 25) {
          streams.push({
            url: details.url,
            mimeType: contentType,
            type: details.url.includes('.m3u8') ? 'hls' : details.url.includes('.mpd') ? 'dash' : 'stream',
            timestamp: Date.now()
          });
          tabMediaMap.set(details.tabId, streams);
          setBadge(details.tabId, `${streams.length}`, '#ec4899');
        }
      }
    },
    { urls: ['<all_urls>'] },
    ['responseHeaders']
  );
}

// ---------------------------------------------------------------------------
// 4. Popup & Frontend Messaging API
// ---------------------------------------------------------------------------
chrome.runtime.onMessage.addListener((request, sender, sendResponse) => {
  if (request.action === 'get_settings') {
    chrome.storage.local.get(DEFAULT_SETTINGS, (items) => {
      sendResponse(items);
    });
    return true;
  }

  if (request.action === 'save_settings') {
    chrome.storage.local.set(request.settings || {}, () => {
      sendResponse({ success: true });
    });
    return true;
  }

  if (request.action === 'get_tab_media') {
    const tabId = request.tabId;
    const media = tabMediaMap.get(tabId) || [];
    sendResponse({ media });
    return true;
  }

  if (request.action === 'native_dispatch') {
    sendNativeMessage(request.payload).then((resp) => {
      sendResponse(resp);
    });
    return true; // Keep message channel open for async response
  }
});
