package io.github.cepheus.app.ui

import io.github.cepheus.app.protocol.AsyncQuestion
import io.github.cepheus.app.protocol.WaitingPrompt









internal object QuestionRecovery {
    /** What the question area shows: the cards when there are any, else the waiting prompt. */
    fun shown(cards: List<AsyncQuestion>, card: AsyncQuestion?, prompt: WaitingPrompt?): Set<String> = when {
        card != null -> cards.mapTo(linkedSetOf()) { "card:${it.id}" }
        prompt != null -> setOf("prompt:${prompt.fp}")
        else -> emptySet()
    }

    /**
     * What a stale refusal sets aside: what was shown, and the waiting prompt
     * on the screen, which would otherwise take the composer again in the
     * place of a card the deck withdrew.
     */
    fun staleAside(shown: Set<String>, waiting: WaitingPrompt?): Set<String> =
        shown + listOfNotNull(waiting?.let { "prompt:${it.fp}" })

    /** Set aside while everything shown was shown when the owner, or a stale refusal, set it aside. */
    fun setAside(shown: Set<String>, aside: Set<String>): Boolean = shown.isNotEmpty() && aside.containsAll(shown)

    /** Whether the composer answers the question rather than messaging the agent. */
    fun answering(prompt: Boolean, card: Boolean, setAside: Boolean, submitted: Boolean): Boolean =
        !setAside && (prompt || (card && !submitted))
}
