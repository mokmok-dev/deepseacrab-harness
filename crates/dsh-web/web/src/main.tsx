import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import { readOrCreateSessionId } from './session';
import './styles.css';

const root = document.getElementById('root');
if (root === null) throw new Error('Missing #root element.');

createRoot(root).render(
  <StrictMode>
    <App initialSessionId={readOrCreateSessionId()} />
  </StrictMode>,
);
