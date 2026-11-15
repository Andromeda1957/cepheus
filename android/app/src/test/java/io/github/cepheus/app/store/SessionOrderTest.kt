package io.github.cepheus.app.store

import io.github.cepheus.app.protocol.CloudAgent
import io.github.cepheus.app.protocol.DeckState
import io.github.cepheus.app.protocol.HostSummary
import io.github.cepheus.app.protocol.Session
import io.github.cepheus.app.protocol.Status
import io.github.cepheus.app.protocol.Usage
import org.junit.Assert.assertEquals
import org.junit.Test

/** The phone keeps the deck's saved order, including moves across machines. */
class SessionOrderTest {
    private fun s(id: Long, alive: Boolean = true) =
        Session(id, "claude", "", if (alive) Status.IDLE else Status.ENDED, "", alive, null, null, null, false, 0, 0)

    // Cached input may still contain ended sessions.
    private val list = listOf(s(1), s(2), s(3), s(4, alive = false), s(5, alive = false))

    private fun ids(sessions: List<Session>) = sessions.map { it.id }

    @Test
    fun aSessionMovesWithinTheList() {
        assertEquals(listOf(3L, 1, 2, 4, 5), ids(SessionOrder.move(list, 3, 0)))
        assertEquals(listOf(2L, 3, 1, 4, 5), ids(SessionOrder.move(list, 1, 2)))
        assertEquals(listOf(1L, 2, 3, 5, 4), ids(SessionOrder.move(list, 5, 3)))
    }

    @Test
    fun aMoveStopsAtTheEdgeOfTheSharedList() {
        assertEquals(listOf(2L, 3, 4, 5, 1), ids(SessionOrder.move(list, 1, 99)))
        assertEquals(listOf(5L, 1, 2, 3, 4), ids(SessionOrder.move(list, 5, -99)))
        assertEquals(emptyList<Session>(), SessionOrder.move(emptyList(), 1, 0))
    }

    @Test
    fun aSessionNoLongerListedChangesNothing() {
        assertEquals(list, SessionOrder.move(list, 9, 0))
        assertEquals(list, SessionOrder.move(list, 2, 1))
    }

    /** A drag shows at once and is put back when the deck refuses it. */
    @Test
    fun anOrderIsAppliedToTheSessionsListedNow() {
        assertEquals(listOf(3L, 1, 2, 4, 5), ids(SessionOrder.arrange(list, listOf(3, 1))))
        assertEquals("a session started meanwhile stays listed", listOf(2L, 1, 3, 4, 5), ids(SessionOrder.arrange(list.reversed(), listOf(2, 1, 9))))
    }

    private fun on(host: String?, id: Long, alive: Boolean = true) = s(id, alive).copy(host = host, hostLabel = host?.uppercase())

    // Mixed-host cached input; a move can cross every machine boundary.
    private val hosted = listOf(on(null, 1), on(null, 2), on("fedora", 3), on("fedora", 4), on("fedora", 5, alive = false), on("ctl", 6))

    @Test
    fun aDragMovesAcrossMachines() {
        assertEquals(listOf(4L, 1, 2, 3, 5, 6), ids(SessionOrder.move(hosted, 4, 0)))
        assertEquals(listOf(1L, 2, 4, 5, 6, 3), ids(SessionOrder.move(hosted, 3, 5)))
        assertEquals(listOf(2L, 3, 4, 5, 1, 6), ids(SessionOrder.move(hosted, 1, 4)))
    }

    private fun deck(sessions: List<Session>) = DeckState(
        sessions = sessions,
        errors = emptyList(),
        startDir = "",
        usage = Usage(emptyList(), emptyList()),
        hosts = listOf(
            HostSummary("fedora", "Fedora", true, null),
            HostSummary("ctl", "Controller", false, "unreachable"),
            HostSummary("spare", "Spare", true, null),
        ),
        hereLabel = "ThinkPad",
    )

    @Test
    fun theSharedSectionKeepsSessionsWhenTheirHostIsOfflineOrNoLongerListed() {
        val state = deck(hosted)
        val expected = listOf(HostGroup(null, "Sessions", null, hosted))
        assertEquals(expected, SessionOrder.groups(state, hosted))
        assertEquals(expected, SessionOrder.groups(state.copy(hosts = state.hosts.take(1)), hosted))
        assertEquals(listOf(HostGroup(null, "Sessions", null, list)), SessionOrder.groups(state.copy(hosts = emptyList()), list))
        // Placement still offers the native machine and only online peers.
        assertEquals(listOf(null, "fedora", "spare"), state.startHosts.map { it.name })
        assertEquals(listOf("ThinkPad", "Fedora", "Spare"), state.startHosts.map { it.label })

        val build = state.copy(hosts = state.hosts + HostSummary("wsl", "WSL", true, null, agents = false))
        assertEquals(listOf(null, "fedora", "spare"), build.startHosts.map { it.name })
    }

    @Test
    fun sharedNativeRowsKeepTheirNumbersAfterLiveCloudRows() {
        val sessions = listOf(
            on(null, 1).copy(tool = "cloud-codex"),
            on(null, 2),
            on(null, 3, alive = false).copy(tool = "cloud-codex"),
            on("fedora", 4).copy(tool = "cloud-codex"),
            on("ctl", 5).copy(tool = "cloud-codex"),
            on("fedora", 6, alive = false),
            on("ctl", 7),
            // Dot is live but goes by its own name: it takes no number.
            on(null, 8).copy(tool = "dot"),
        )
        val state = deck(sessions).copy(cloudAgents = listOf(
            CloudAgent("cloud-codex", "Cloud"),
            CloudAgent("cloud-codex", "Cloud Fedora", "fedora"),
            CloudAgent("dot", "Dot"),
        ))
        // The shell filters cloud identities by both provider and host.
        val native = sessions.filterNot { state.isCloud(it) }
        val shown = SessionOrder.shown(state, native)
        assertEquals(listOf(listOf(2L, 5, 7)), shown.map { (group, _) -> ids(group.sessions) })
        assertEquals(listOf(listOf(3, 4, 5)), shown.map { (_, rows) -> rows.map { it.number } })
    }


    @Test
    fun theListsShowRunningSessionsUnderTheDecksOwnNumbers() {
        val sessions = listOf(on(null, 1), on(null, 2, alive = false), on("fedora", 3), on("ctl", 4, alive = false))
        val shown = SessionOrder.shown(deck(sessions), sessions)
        assertEquals(listOf(listOf(1L, 3)), shown.map { (g, _) -> ids(g.sessions) })
        assertEquals(listOf(listOf(1, 2)), shown.map { (_, rows) -> rows.map { it.number } })
    }
}
