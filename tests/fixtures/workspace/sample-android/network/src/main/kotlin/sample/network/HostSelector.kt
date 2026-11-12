package sample.network

import sample.core.Analytics

/**
 * Chooses which backend URL the app talks to.
 * A host that worked before wins; otherwise the first default host is used.
 * A failed host is skipped until every host has failed.
 */
class HostSelector(
    private val defaults: List<String>,
    private val store: HostStore,
    private val analytics: Analytics,
) {
    private val failed = mutableSetOf<String>()

    fun pick(): String {
        val stored = store.load()
        if (stored != null && stored !in failed) {
            return stored
        }
        return defaults.firstOrNull { it !in failed } ?: defaults.first()
    }

    fun markFailed(host: String) {
        failed += host
        val next = pick()
        if (next != host) {
            store.save(next)
            analytics.track("host_switched", mapOf("from" to host, "to" to next))
        }
    }
}
