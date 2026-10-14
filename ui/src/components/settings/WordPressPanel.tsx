// Settings → WordPress: server-wide defaults for new sites (settings.wordpress) and crawl
// limits (settings.crawl). Per-site credentials live in WpCredentialsForm.

import type { CrawlSettings, ServerSettings, WordpressSettings } from '../../types2';
import { Toggle } from '../Toggle';
import { NumInput, RestartBanner, Row, SaveBar, SettingsGate, useDraft, useServerSettings, type ServerSettingsState } from './serverSettings';

const MODULES: Array<{ id: string; label: string; desc: string }> = [
  { id: 'native', label: 'Native', desc: 'Link existing words in the paragraph.' },
  { id: 'rephrase', label: 'Rephrase', desc: 'Lightly rewrite a sentence to fit the anchor.' },
  { id: 'context', label: 'Context', desc: 'Add a short linking sentence.' },
  { id: 'content_gap', label: 'Content gap', desc: 'Add a new sentence where a topic is missing.' },
];

function DefaultsCard({ state, s }: { state: ServerSettingsState; s: ServerSettings }) {
  const w = useDraft<WordpressSettings>(s.wordpress);
  if (!s.wordpress) return null;
  const d = w.draft;
  const mods = d.default_insert_modules ?? [];
  return (
    <div className="card">
      <div className="card-title">Defaults for every site</div>
      <p className="muted card-lead">Used when a site has not set its own value under Linking Rules.</p>
      <div className="settings-list">
        <Row label="Insert modules" desc="How links may be placed into content.">
          <div className="check-col">
            {MODULES.map((m) => (
              <label key={m.id} className="check-line">
                <input
                  type="checkbox"
                  checked={mods.includes(m.id)}
                  onChange={(e) => w.set('default_insert_modules', e.target.checked ? [...mods, m.id] : mods.filter((x) => x !== m.id))}
                />
                <span>
                  <b>{m.label}</b> <span className="faint">{m.desc}</span>
                </span>
              </label>
            ))}
          </div>
        </Row>
        <Row label="Verify after inject" desc="Re-read each changed post to confirm the link is live.">
          <Toggle checked={d.verify_after_inject} onChange={(v) => w.set('verify_after_inject', v)} label="Verify after inject" />
        </Row>
        <Row label="Maximum links per post" desc="Ceiling on contextual internal links in one post.">
          <NumInput label="Maximum links per post" value={d.default_max_links_per_post} onChange={(v) => w.set('default_max_links_per_post', v)} min={0} max={1000} />
        </Row>
        <Row label="Minimum links per post" desc="Posts below this are flagged as under-linked. 0 turns the flag off.">
          <NumInput label="Minimum links per post" value={d.default_min_links_per_post} onChange={(v) => w.set('default_min_links_per_post', v)} min={0} max={1000} />
        </Row>
        <Row label="New links per pass" desc="How many links one injection run may add to a single post.">
          <NumInput label="New links per pass" value={d.max_new_links_per_pass} onChange={(v) => w.set('max_new_links_per_pass', v)} min={1} max={100} />
        </Row>
        <Row label="Request timeout" desc="How long to wait for the WordPress REST API.">
          <NumInput label="Request timeout" value={d.request_timeout_ms} onChange={(v) => w.set('request_timeout_ms', v)} min={1000} max={600000} unit="ms" />
        </Row>
        <Row label="User agent" desc="Sent with every request to your sites. Useful for firewall allow rules." wide>
          <input className="input" value={d.user_agent} onChange={(e) => w.set('user_agent', e.target.value)} aria-label="User agent" />
        </Row>
      </div>
      <SaveBar dirty={w.dirty} saving={state.saving} onReset={w.reset} onSave={() => void state.save({ wordpress: w.changes as Record<string, unknown> }, 'WordPress defaults saved')} />
    </div>
  );
}

function CrawlCard({ state, s }: { state: ServerSettingsState; s: ServerSettings }) {
  const c = useDraft<CrawlSettings>(s.crawl);
  if (!s.crawl) return null;
  const d = c.draft;
  return (
    <div className="card">
      <div className="card-title">Crawling</div>
      <div className="settings-list">
        <Row label="Parallel requests" desc="Requests sent to one site at the same time. Lower this for small hosting plans.">
          <NumInput label="Parallel requests" value={d.concurrency} onChange={(v) => c.set('concurrency', v)} min={1} max={32} />
        </Row>
        <Row label="Page timeout" desc="Give up on a page after this long.">
          <NumInput label="Page timeout" value={d.timeout_ms} onChange={(v) => c.set('timeout_ms', v)} min={1000} max={600000} unit="ms" />
        </Row>
        <Row label="Maximum pages" desc="Stop a crawl after this many pages.">
          <NumInput label="Maximum pages" value={d.max_pages} onChange={(v) => c.set('max_pages', v)} min={1} max={10_000_000} />
        </Row>
        <Row label="Allow private addresses" desc="Let crawls reach LAN or localhost sites (for staging copies). Leave off for public sites.">
          <Toggle checked={d.allow_private_hosts} onChange={(v) => c.set('allow_private_hosts', v)} label="Allow private addresses" />
        </Row>
      </div>
      <SaveBar dirty={c.dirty} saving={state.saving} onReset={c.reset} onSave={() => void state.save({ crawl: c.changes as Record<string, unknown> }, 'Crawl settings saved')} />
    </div>
  );
}

/** Server-wide WordPress + crawl cards. Non-admins see nothing (the gate shows "Admins only"). */
export function WordPressServerCards() {
  const state = useServerSettings();
  return (
    <>
      <RestartBanner state={state} />
      <SettingsGate state={state}>
        {(s) => (
          <>
            <DefaultsCard state={state} s={s} />
            <CrawlCard state={state} s={s} />
          </>
        )}
      </SettingsGate>
    </>
  );
}
