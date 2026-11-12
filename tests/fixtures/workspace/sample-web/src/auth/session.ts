import { fetchWithRetry } from "../http/fetchWithRetry";

export interface Session {
  accessToken: string;
  refreshToken: string;
  expiresAt: number;
}

/** Returns a session whose access token is still valid, refreshing it when it expires. */
export async function ensureFreshSession(session: Session, now = Date.now()): Promise<Session> {
  if (session.expiresAt > now + 30_000) {
    return session;
  }
  const response = await fetchWithRetry(`/auth/refresh?token=${session.refreshToken}`);
  const body = (await response.json()) as { accessToken: string; expiresIn: number };
  return {
    accessToken: body.accessToken,
    refreshToken: session.refreshToken,
    expiresAt: now + body.expiresIn * 1000,
  };
}
