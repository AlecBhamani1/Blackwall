import { writable } from 'svelte/store';
import { invoke } from '@tauri-apps/api/core';
export type WebSettings = {
  provider: 'duckduckgo' | 'brave';
  braveFallback: boolean;
  rendering: boolean;
};
export type WebView = {
  settings: WebSettings;
  hasKey: boolean;
  browserSupported: boolean;
  browserInstalled: boolean;
};
export const webClient = {
  load: () => invoke<WebView>('load_web_settings'),
  save: (settings: WebSettings) => invoke<void>('save_web_settings', { settings }),
  saveKey: (key: string) => invoke<void>('save_search_key', { key }),
  check: () => invoke<void>('check_web_search'),
  install: () => invoke<void>('install_web_browser'),
  open: (url: string) => invoke<void>('open_web_source', { url }),
};

export const webSettingsRequested = writable(false);
export const webRenderRequested = writable('');
