package io.github.cepheus.app.protocol

/** What goes back in `answer`: an option, its typed text, and for a several-choice question every box to leave ticked. */
data class Answer(val choice: Int, val text: String?, val choices: List<Int> = emptyList())

/** Why the deck took no answer; `stale` when its question no longer waits ([Protocol.QUESTION_STALE]). */
data class AnswerRefusal(val message: String, val stale: Boolean)








object Answers {
    /** The boxes ticked now: a several-choice card starts with them. */
    fun ticked(prompt: Prompt): Set<Int> =
        prompt.options.indices.filterTo(mutableSetOf()) { prompt.options[it].checked == true }

    /** Whether option `i` is a box sent with the others, rather than an answer on its own. */
    fun isBox(prompt: Prompt, i: Int): Boolean = prompt.multi && prompt.options.getOrNull(i)?.checked != null

    /**
     * A several-choice answer: the boxes in `picks`, with `text` for the
     * free-text box. Null while it cannot be sent: nothing picked, the
     * free-text box picked without text, or no Submit row on the screen.
     */
    fun several(prompt: Prompt, picks: Set<Int>, text: String?): Answer? {
        if (!prompt.multi || prompt.submit == null) return null
        val boxes = picks.filter { isBox(prompt, it) }.sorted()
        if (boxes.isEmpty()) return null
        val free = prompt.freeText?.takeIf { it in boxes }
        val typed = text?.trim()?.takeIf { it.isNotEmpty() }
        if (free != null && typed == null) return null
        return Answer(boxes.first(), typed.takeIf { free != null }, boxes)
    }

    /**
     * Text typed in the message box while a question waits answers it through
     * the question's free-text option, with any boxes picked on its card. Null
     * when the question takes no typed answer: then the box waits for a tap.
     */
    fun typed(prompt: Prompt, picks: Set<Int>, text: String): Answer? {
        val free = prompt.freeText ?: return null
        val typed = text.trim().takeIf { it.isNotEmpty() } ?: return null
        return if (prompt.multi) several(prompt, picks + free, typed) else Answer(free, typed)
    }
}
