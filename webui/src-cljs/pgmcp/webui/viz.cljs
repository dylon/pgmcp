(ns pgmcp.webui.viz
  "Pure geometry for hand-authored SVG charts — numbers to path strings and
  scales, no reagent/DOM. Keeps chart views free of ad-hoc math and keeps this
  namespace within the gate's purity rules (js/Math is used elsewhere in the
  pure layer too). CSP-safe: no charting library, no eval."
  (:require [clojure.string :as str]))

(defn nice-max
  "Round a data max up to a 'nice' axis bound (1/2/5 × 10^n)."
  [v]
  (if (<= v 0)
    1
    (let [mag (js/Math.pow 10 (js/Math.floor (/ (js/Math.log v) (js/Math.log 10))))
          n (/ v mag)]
      (* mag (cond (<= n 1) 1 (<= n 2) 2 (<= n 5) 5 :else 10)))))

(defn linear
  "A linear scale from data domain [d0,d1] to pixel range [r0,r1]."
  [d0 d1 r0 r1]
  (let [span (- d1 d0)]
    (fn [v] (if (zero? span) r0 (+ r0 (* (/ (- v d0) span) (- r1 r0)))))))

(defn line-path
  "points = seq of [x y] in SVG coords → an SVG path `d` string."
  [points]
  (->> points
       (map-indexed (fn [i [x y]]
                      (str (if (zero? i) "M" "L") (.toFixed x 1) " " (.toFixed y 1))))
       (str/join " ")))

(defn series->points
  "Map a value sequence to SVG coords within a w×h box (padding `pad`), y scaled
  to [0, ymax] with 0 at the bottom."
  [values w h pad ymax]
  (let [n (count values)
        xs (if (<= n 1) (constantly (/ w 2)) (linear 0 (dec n) pad (- w pad)))
        ys (linear 0 (max ymax 1) (- h pad) pad)]
    (map-indexed (fn [i v] [(xs i) (ys (or v 0))]) values)))

(defn bars
  "Bar rectangles for a value sequence: seq of {:x :y :w :h} in SVG coords."
  [values w h pad ymax]
  (let [n (count values)
        ys (linear 0 (max ymax 1) (- h pad) pad)
        avail (- w (* 2 pad))
        slot (if (pos? n) (/ avail n) avail)
        bw (max 1 (* slot 0.7))]
    (map-indexed
     (fn [i v]
       (let [x (+ pad (* i slot) (/ (- slot bw) 2))
             y (ys (or v 0))]
         {:x x :y y :w bw :h (max 0 (- (- h pad) y))}))
     values)))
