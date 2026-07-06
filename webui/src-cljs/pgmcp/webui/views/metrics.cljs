(ns pgmcp.webui.views.metrics
  "Grafana-style metrics dashboard — time-bucketed series over tool-call
  telemetry, cron runs, and quality history, rendered as an SVG chart + table."
  (:require [pgmcp.webui.domain :as domain]
            [pgmcp.webui.views.common :as ui]
            [pgmcp.webui.views.panel :as panel]
            [re-frame.core :as rf]))

(defn metrics-url [series]
  (str "/api/metrics?series=" series "&bucket=hour&since_minutes=1440"))

(defn series-controls [series]
  (into [:span.chips-row]
        (for [s ["tool_calls" "cron" "quality"]]
          [ui/toolbar-button
           {:label s
            :active? (= s series)
            :on-click (fn []
                        (rf/dispatch [:ui/set-panel-param :metrics :series s])
                        (panel/load! :metrics (metrics-url s)))}])))

(defn metrics-page []
  (let [series @(rf/subscribe [:panel/ui-param :metrics :series "tool_calls"])]
    [panel/data-panel
     {:id :metrics
      :url (metrics-url series)
      :normalizer domain/normalized-metrics
      :controls [series-controls series]}]))
