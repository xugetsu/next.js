import { inc, double, log } from './actions'

export default function Page() {
  return (
    <>
      <button id="inc" onClick={inc}>
        inc
      </button>
      <button id="double" onClick={double}>
        double
      </button>
      <button id="log" onClick={log}>
        log
      </button>
    </>
  )
}
