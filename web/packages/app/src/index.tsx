/* @refresh reload */
import { render } from 'solid-js/web';
import './styles/global.css';
import { App } from './App';
import { installDevConsoleErrorListeners } from './lib/devConsoleErrors';

installDevConsoleErrorListeners();

const root = document.getElementById('root');
if (!root) {
  throw new Error('#root element not found');
}
render(() => <App />, root);
