import { mount } from 'svelte';
import App from './App.svelte';
import { guardLinks } from './lib/externalLinks';
import { isDesktop } from './lib/setup';
import { webClient } from './lib/web';
import './theme.css';

const target = document.getElementById('app');

if (!target) {
  throw new Error('Blackwall could not find its application mount point.');
}

guardLinks(document, (url) =>
  isDesktop() ? webClient.open(url) : window.open(url, '_blank', 'noopener,noreferrer'),
);
mount(App, { target });
