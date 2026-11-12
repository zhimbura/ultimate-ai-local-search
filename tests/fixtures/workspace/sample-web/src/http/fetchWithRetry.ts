/**
 * Calls fetch and retries network errors and 5xx answers.
 * The wait doubles after each failed attempt.
 */
export async function fetchWithRetry(
  url: string,
  attempts = 3,
  baseDelayMs = 200,
): Promise<Response> {
  let lastError: unknown;
  for (let attempt = 0; attempt < attempts; attempt++) {
    try {
      const response = await fetch(url);
      if (response.status < 500) {
        return response;
      }
      lastError = new Error(`server error ${response.status}`);
    } catch (error) {
      lastError = error;
    }
    await new Promise((resolve) => setTimeout(resolve, baseDelayMs * 2 ** attempt));
  }
  throw lastError;
}
