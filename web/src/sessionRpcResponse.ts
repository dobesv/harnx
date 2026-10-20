import type { JsonRpcResponse } from './types';

export async function sessionRpcResponse<T>(response: Response, fallback: T): Promise<T> {
  let json: JsonRpcResponse<T> | undefined;
  try {
    json = await response.json() as JsonRpcResponse<T>;
  } catch {
    json = undefined;
  }
  // Control-plane errors can be more specific than the HTTP status.
  if (json?.error) throw new Error(`RPC Error: ${json.error.message || json.error.code}`);
  if (!response.ok) throw new Error(`RPC call failed with HTTP ${response.status}`);
  return json?.result || fallback;
}
