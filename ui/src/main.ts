import { mount } from 'svelte';
import App from './App.svelte';
import { takeTokenFromUrl } from './lib/token';
import './styles/tokens.css';

takeTokenFromUrl(window.location, (url) => window.history.replaceState(null, '', url));

const target = document.getElementById('app');
if (target) mount(App, { target });
