import type { Register } from 'claude-code'

/** This plugin's name, as Claude stamps the prompts it submits. */
const PLUGIN = 'cepheus-owner-input'
/**
 * How long a prompt has to start or be refused before it is reported as
 * merely submitted: a prompt Claude refuses at once (a Stop stands) is then
 * the send's own error on the phone, not a later one.
 */
const SETTLE_MS = 1500
/** Why a held message was not delivered: the owner's Stop ended its turn. */
const STOPPED = 'the turn was stopped before the message reached it'

type Outcome = [outcome: string, reason?: string]

/**
 * An owner message the service asked to steer into the running main turn
 * (a broadcast). Claude runs a plugin's own prompt only once the session is
 * idle, so it is held for the turn's next tool result and read with it.
 */
type Held = {
  text: string
  files?: string[]
  /** Settles the message's outcome, or adopts its fallback submission. */
  settle: (outcome: Outcome | Promise<Outcome>) => void
  /** Submits it as a turn of its own, when its turn ends before a tool result. */
  submit: () => Promise<Outcome>
}

/** What tells Claude to open the files the owner attached. */
const filesNote = (files: string[]) =>
  `The owner attached ${files.length === 1 ? 'this file' : 'these files'} from the Cepheus phone app. ` +
  `Open each one with the Read tool before answering:\n${files.join('\n')}`

export const register: Register = on => {
  // The main loop's running turn, which the owner's Stop ends.
  let running: string | undefined
  const held: Held[] = []
  on('turn.start', ($, e, next) => {
    if (e.agentId === undefined) running = e.turnId
    return next(e)
  })
  on('turn.complete', ($, e, next) => {
    if (e.agentId === undefined && running === e.turnId) {
      running = undefined
      // The turn ended before another tool result: a stopped turn stays
      // stopped, and any other end leaves the message its own turn.
      for (const message of held.splice(0)) message.settle(e.isAborted ? ['dropped', STOPPED] : message.submit())
    }
    return next(e)
  })

  // A held message joins the running turn with the next result of a tool the
  // model called in the main loop, as a prompt typed mid-turn does.
  on('tool.call', async ($, e, next) => {
    const result = await next(e)
    if (e.agentId !== undefined || next.origin.plugin !== 'engine' || result.deny !== undefined || !held.length) {
      return result
    }
    const taken = held.splice(0)
    for (const message of taken) message.settle(['steered'])
    const context = taken.flatMap(({ text, files }) => (files?.length ? [text, filesNote(files)] : [text]))
    return { ...result, context: [...(result.context ?? []), ...context] }
  })

  // Files the owner attached, by the text of the prompt that names them:
  // Claude is told to open them, since a plugin's prompt carries no pictures.
  const attached = new Map<string, string[][]>()
  on('prompt.submit', ($, e, next) => {
    const mine = e.origin?.kind === 'plugin' && e.origin.name === PLUGIN
    const queue = mine ? attached.get(e.text) : undefined
    const files = queue?.shift()
    if (queue?.length === 0) attached.delete(e.text)
    if (!files?.length) return next(e)
    return next({ ...e, context: [...(e.context ?? []), filesNote(files)] })
  })

  on('session.start', async ($, e, next) => {
    const started = await next(e)
    const program = await $.env.get('CEPHEUS_OWNER_INPUT')
    if (!program) return started
    const receipt = (message: string, outcome: string, reason?: string) =>
      $.process
        .run([program, 'hook', 'owner-input', 'receipt'], { stdin: JSON.stringify({ message, outcome, reason }) })
        .catch(() => undefined)
    // Report a prompt's start or refusal; one still waiting for Claude to go
    // idle, or for its turn's next tool result, is first reported submitted,
    // and its outcome when it comes.
    const settle = async (message: string, entered: Promise<Outcome>) => {
      const waited = $.clock.sleep(SETTLE_MS).then(
        () => undefined,
        () => undefined,
      )
      const early = await Promise.race([entered, waited])
      if (early) return receipt(message, ...early)
      await receipt(message, 'submitted')
      return receipt(message, ...(await entered))
    }
    const stop = async (message: string) => {
      const turnId = running
      if (turnId === undefined) return receipt(message, 'idle')
      return $.turn.abort({ turnId }).then(
        () => receipt(message, 'stopped'),
        error => receipt(message, 'failed', String(error)),
      )
    }
    const submit = (prompt: string, files?: string[]) => {
      if (files?.length) attached.set(prompt, [...(attached.get(prompt) ?? []), files])
      const entered = $.prompt.submit({ text: prompt, asUser: true }).then(
        (result): Outcome => (result.drop === undefined ? ['started'] : ['dropped', result.drop]),
        (error): Outcome => ['failed', String(error)],
      )
      // A prompt refused before it entered leaves no files to describe.
      void entered.then(() => {
        const left = attached.get(prompt)?.filter(listed => listed !== files)
        if (left?.length) attached.set(prompt, left)
        else attached.delete(prompt)
      })
      return entered
    }
    void (async () => {
      const argv = [program, 'hook', 'owner-input', 'listen', await $.session.id(), 'stop']
      const listen = $.process.spawn({ argv })
      let lines = ''
      for await (const { stream, text } of listen) {
        if (stream !== 'stdout') continue
        lines += text
        for (let end = lines.indexOf('\n'); end >= 0; end = lines.indexOf('\n')) {
          const line = lines.slice(0, end)
          lines = lines.slice(end + 1)
          let request: {
            message: string
            conversation?: string
            text?: string
            files?: string[]
            stop?: boolean
            steer?: boolean
          }
          try {
            request = JSON.parse(line)
          } catch {
            continue
          }
          const { message, conversation, text: prompt, files, stop: stopping, steer } = request
          if (stopping) {
            void stop(message)
            continue
          }
          if (typeof prompt !== 'string') continue
          if ((await $.session.id()) !== conversation) {
            await receipt(message, 'moved')
            continue
          }
          // Held or submitted in the order the service sent them; with no
          // main turn running, a steered message starts one, as any other.
          const entered =
            steer === true && running !== undefined
              ? new Promise<Outcome>(settle =>
                  held.push({ text: prompt, files, settle, submit: () => submit(prompt, files) }),
                )
              : submit(prompt, files)
          void settle(message, entered)
        }
      }
    })()
    return started
  })
}
