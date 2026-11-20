// The template's own scenario: what every copy serves from the first day. 90% token-
// authenticated reads of a workspace route (the API token lookup, the access check, the
// whole middleware stack), 10% liveness probes, offered at a constant arrival rate (an open
// model: k6 keeps offering load whether or not the server keeps up, and counts what it could
// not send as dropped). A copy adds a scenario per feature it adds.
//
// Env: BASE_URL, TOKEN, WORKSPACE, MODE (smoke | full), RATE (requests/s); TOTAL for
// plumbing probes only (scripts/load.sh keeps those out of perf/results).
import http from 'k6/http';
import { check } from 'k6';
import { Counter, Rate } from 'k6/metrics';

const MODE = __ENV.MODE || 'smoke';
const RATE = parseInt(__ENV.RATE || '1000', 10);
const TOTAL = __ENV.TOTAL ? parseInt(__ENV.TOTAL, 10) : MODE === 'full' ? 1000000 : 100000;
const BASE = __ENV.BASE_URL;
const READ = `${BASE}/v1/workspaces/${__ENV.WORKSPACE}/hello`;
const AUTH = { headers: { Authorization: `Bearer ${__ENV.TOKEN}` }, tags: { kind: 'read' } };

const shed = new Rate('shed'); // 503 overloaded: load shedding, counted apart
const unexpected = new Counter('unexpected_status');

export const options = {
  discardResponseBodies: true,
  summaryTrendStats: ['min', 'med', 'avg', 'p(90)', 'p(95)', 'p(99)', 'max'],
  scenarios: {
    service: {
      executor: 'constant-arrival-rate',
      rate: RATE,
      timeUnit: '1s',
      duration: `${Math.ceil(TOTAL / RATE)}s`,
      preAllocatedVUs: 100,
      maxVUs: 1000,
    },
  },
  thresholds: {
    // Chosen, not measured: the first quiet-machine run sets them.
    http_req_failed: ['rate<0.001'],
    'http_req_duration{kind:read}': ['p(99)<50'],
    shed: ['rate<0.001'],
    unexpected_status: ['count<1'],
    dropped_iterations: ['count<1'],
  },
};

export default function () {
  const read = Math.random() < 0.9;
  const res = read ? http.get(READ, AUTH) : http.get(`${BASE}/livez`, { tags: { kind: 'live' } });
  shed.add(res.status === 503);
  if (!check(res, { 'status 200': (r) => r.status === 200 })) {
    if (res.status !== 503) unexpected.add(1);
  }
}
