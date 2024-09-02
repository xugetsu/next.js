import { inc, double, log } from './actions'

export default function Page() {
  return (
    <>
      <button id="inc" onClick={() => console.log(inc(1))}>
        inc
      </button>
      <button id="double" onClick={() => console.log(double(1))}>
        double
      </button>
      <button id="log" onClick={() => log(1)}>
        log
      </button>
    </>
  )
}
