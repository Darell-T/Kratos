import { loadConfig } from './config';
import { Hub } from './hub';
import { startServer } from './server';
import { Store } from './store';

const config = loadConfig();
const store = new Store(config.home);
const server = await startServer({ store, hub: new Hub(), uiDist: config.uiDist, port: config.port });

console.log(`Kratos engine listening on http://127.0.0.1:${server.port} (data in ${config.home})`);

async function shutdown(): Promise<void> {
  try {
    await server.close();
    store.close();
  } catch (error) {
    console.error(error);
    process.exitCode = 1;
  }
}

process.once('SIGINT', () => void shutdown());
process.once('SIGTERM', () => void shutdown());
