(ns pgmcp.webui.views.widgets
  "Presentational, hiccup-only building blocks fed by already-normalized data
  (see pgmcp.webui.domain). No re-frame dispatch, no edge APIs — pure rendering
  so the JSON-dump renderer can be retired in favor of purpose-built surfaces.

  A `section` is the generic container the panes compose: a titled box whose
  body is one of :tiles / :table / :kv / :meters / :chips / :chart / :custom.
  Normalizers return a vector of section maps and a pane renders them with
  `sections`."
  (:require [clojure.string :as str]
            [pgmcp.webui.viz :as viz]))

(defn chip [{:keys [label status]}]
  [:span {:class (str "chip " (name (or status :neutral)))} (str label)])

(defn chips [items]
  (into [:span.chips-row]
        (for [[idx item] (map-indexed vector items)]
          ^{:key idx}
          (if (map? item) (chip item) (chip {:label item})))))

(defn stat-tile [{:keys [label value sub status]}]
  [:div {:class (str "stat-tile" (when status (str " " (name status))))}
   [:div.label (str label)]
   [:div.value (str value)]
   (when-not (str/blank? (str sub))
     [:div.sub (str sub)])])

(defn tiles [items]
  (into [:div.tiles]
        (for [[idx t] (map-indexed vector items)]
          ^{:key idx} [stat-tile t])))

(defn kv-grid [pairs]
  (into [:div.kv-grid]
        (mapcat (fn [[idx [k v]]]
                  [^{:key (str "k" idx)} [:div.k (str k)]
                   ^{:key (str "v" idx)} [:div.v (str v)]])
                (map-indexed vector pairs))))

(defn meter
  "A horizontal fill meter. `fraction` in [0,1]; `status` colors the fill
  (:ok / :warn / :danger). `label` overlays a value string."
  [{:keys [fraction label status]}]
  (let [pct (-> (or fraction 0) (max 0) (min 1) (* 100))]
    [:div {:class (str "meter" (when status (str " " (name status))))}
     [:div.fill {:style {:width (str pct "%")}}]
     (when label [:div.meter-label (str label)])]))

(defn meter-row [{:keys [name] :as m}]
  [:div.meter-row
   [:span (str name)]
   [meter m]])

(defn meters [items]
  (into [:div.meters]
        (for [[idx m] (map-indexed vector items)]
          ^{:key idx} [meter-row m])))

(defn chart
  "A single-series SVG chart that scales to container width via viewBox.
  spec: {:type :line|:bar :values [n...] :caption <str>}."
  [{:keys [type values caption]}]
  (let [h 90 w 600 pad 8
        vs (mapv #(or % 0) values)
        ymax (viz/nice-max (apply max 1 vs))]
    [:div.chart-wrap
     [:svg.chart {:viewBox (str "0 0 " w " " h)
                  :preserveAspectRatio "none"
                  :style {:width "100%" :height (str h "px")}}
      [:line {:x1 pad :y1 (- h pad) :x2 (- w pad) :y2 (- h pad)
              :stroke "var(--vv-border)" :stroke-width 1}]
      (if (= type :bar)
        (into [:g]
              (for [[idx b] (map-indexed vector (viz/bars vs w h pad ymax))]
                ^{:key idx}
                [:rect {:x (:x b) :y (:y b) :width (:w b) :height (:h b)
                        :fill "var(--vv-series-1)"}]))
        [:path {:d (viz/line-path (viz/series->points vs w h pad ymax))
                :fill "none" :stroke "var(--vv-series-1)" :stroke-width 2}])]
     (when caption [:div.chart-caption caption])]))

(defn cell
  "Render a domain-produced table cell value. A {:chip <label> :status <s>} map
  becomes a status chip; anything else stringifies. This keeps the pure domain
  normalizers free of any view dependency — they emit data, this renders it.
  View-composed tables that need richer cells use a column :render fn instead."
  [v]
  (if (and (map? v) (contains? v :chip))
    (chip {:label (:chip v) :status (:status v)})
    (str v)))

(defn data-table
  "columns: [{:key <k> :label <str> :align :num|nil :render (fn [row] hiccup)}]
  rows: seq of maps. A column's :render, when present, produces a typed cell;
  otherwise the value is rendered by `cell` (chip data or stringified)."
  [{:keys [columns rows empty-text]}]
  (if (empty? rows)
    [:div.empty (or empty-text "No rows.")]
    [:div.table-scroll
     [:table.data-table
      [:thead
       (into [:tr]
             (for [[idx c] (map-indexed vector columns)]
               ^{:key idx}
               [:th {:class (when (= :num (:align c)) "num")} (str (:label c))]))]
      (into [:tbody]
            (for [[ridx row] (map-indexed vector rows)]
              ^{:key ridx}
              (into [:tr]
                    (for [[cidx c] (map-indexed vector columns)]
                      ^{:key cidx}
                      [:td {:class (when (= :num (:align c)) "num")}
                       (if-let [render (:render c)]
                         (render row)
                         (cell (get row (:key c))))]))))]]))

(defn section-body [s]
  (cond
    (contains? s :tiles) (tiles (:tiles s))
    (contains? s :table) (data-table (:table s))
    (contains? s :kv) (kv-grid (:kv s))
    (contains? s :meters) (meters (:meters s))
    (contains? s :chips) (chips (:chips s))
    (contains? s :chart) (chart (:chart s))
    (contains? s :custom) (:custom s)
    :else nil))

(defn section [{:keys [title actions] :as s}]
  [:section.section
   (when (or title actions)
     [:h2.section-title
      [:span (str title)]
      (when actions actions)])
   [:div.section-body (section-body s)]])

(defn sections [ss]
  (into [:div.sections]
        (for [[idx s] (map-indexed vector (remove nil? ss))]
          ^{:key idx} [section s])))
