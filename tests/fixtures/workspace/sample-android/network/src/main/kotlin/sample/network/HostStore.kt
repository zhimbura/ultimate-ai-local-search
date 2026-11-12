package sample.network

/** Remembers the backend host that answered last time. */
interface HostStore {
    fun load(): String?
    fun save(host: String)
}

class InMemoryHostStore : HostStore {
    private var host: String? = null

    override fun load(): String? = host

    override fun save(host: String) {
        this.host = host
    }
}
