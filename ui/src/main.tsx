import React from 'react';
import { createRoot } from 'react-dom/client';
import App from './App';
import { HintProvider } from './hints';
import { applyTheme, readTheme } from './theme';
import 'reactflow/dist/style.css';
import './styles.css';

applyTheme(readTheme());

const el = document.getElementById('root');
if (!el) throw new Error('#root element missing');

createRoot(el).render(
  <React.StrictMode>
    <HintProvider>
      <App />
    </HintProvider>
  </React.StrictMode>,
);
