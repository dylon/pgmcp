(ns pgmcp.webui.views.experiments
  "Scientific experiment ledgers: a filterable registry with per-experiment
  drill-down (hypotheses / measurements / decisions / artifacts / timeline) and
  the rendered Markdown ledger. Reuses the panel fetch machinery, the widget
  sections renderer, and markdown-view."
  (:require [clojure.string :as str]
            [pgmcp.webui.domain :as domain]
            [pgmcp.webui.views.common :as ui]
            [pgmcp.webui.views.markdown :as markdown]
            [pgmcp.webui.views.panel :as panel]
            [pgmcp.webui.views.widgets :as w]
            [re-com.core :as rc]
            [re-frame.core :as rf]
            [reagent.core :as r]))

(def status-filter-choices
  (mapv (fn [s] {:id s :label (if (= s "") "all statuses" s)})
        ["" "open" "in_progress" "decided" "verified" "rejected" "superseded"]))

(defn qparam [k v]
  (when-not (str/blank? (str v)) (str "&" k "=" (js/encodeURIComponent v))))

(defn list-url [status project]
  (str "/api/experiments?limit=100" (qparam "status" status) (qparam "project" project)))

(defn set-param! [key value]
  (rf/dispatch [:ui/set-panel-param :experiments key value]))

(defn experiments-list [status project selected]
  (r/with-let [_ (panel/load! :experiments (list-url status project))]
    (let [payload @(rf/subscribe [:panel/payload :experiments])
          pending? @(rf/subscribe [:panel/pending? :experiments])
          rows (:experiments payload)]
      [:div.exp-list
       [:div.toolbar
        [rc/single-dropdown
         :choices status-filter-choices
         :model (or status "")
         :width "140px"
         :on-change #(set-param! :status %)]
        [rc/input-text
         :class "mandate-project"
         :model (or project "")
         :placeholder "project"
         :change-on-blur? false
         :on-change #(set-param! :project %)]
        [ui/toolbar-button
         {:label (if pending? "Loading…" "Load")
          :disabled? pending?
          :on-click #(panel/load! :experiments (list-url status project))}]]
       (cond
         (nil? payload)
         [ui/empty-box (if pending? "Loading experiments…" "No experiments loaded.")]

         (:error payload)
         [ui/error-box (:error payload)]

         (seq rows)
         [:div.results
          (for [[idx x] (map-indexed vector rows)]
            ^{:key (str (:slug x) ":" idx)}
            [:div.exp-row {:class (when (= selected (:slug x)) "exp-selected")}
             [ui/meta-row (:slug x) (:kind x) (:status x) (or (:project x) "unassigned")]
             [:div.work-title (or (:title x) (:slug x))]
             [ui/toolbar-button
              {:label (if (= selected (:slug x)) "Open ✓" "Open")
               :on-click #(set-param! :slug (:slug x))}]])]

         :else
         [ui/empty-box "No experiments."])])))

(defn experiment-ledger [slug]
  (r/with-let [_ (panel/load! :experiment-ledger
                              (str "/api/experiments/" (js/encodeURIComponent slug) "/ledger"))]
    (let [payload @(rf/subscribe [:panel/payload :experiment-ledger])]
      (cond
        (nil? payload) [ui/empty-box "Loading ledger…"]
        (:error payload) [ui/error-box (:error payload)]
        (not (str/blank? (:ledger payload)))
        [markdown/markdown-view (str "exp-ledger:" slug) (:ledger payload)]
        :else [ui/empty-box "No ledger."]))))

(defn experiment-detail [slug]
  (r/with-let [_ (panel/load! :experiment-detail (str "/api/experiments/" (js/encodeURIComponent slug)))]
    (let [payload @(rf/subscribe [:panel/payload :experiment-detail])
          show-ledger @(rf/subscribe [:panel/ui-param :experiment-detail :ledger false])
          item (:experiment payload)
          assign @(rf/subscribe [:panel/ui-param :experiment-detail :assign-project ""])
          act @(rf/subscribe [:action/status (str "exp-assign:" slug)])]
      [:div.exp-detail
       [:div.toolbar
        [ui/toolbar-button {:label "Close" :on-click #(set-param! :slug nil)}]
        [ui/toolbar-button
         {:label (if show-ledger "Detail" "Ledger")
          :on-click #(rf/dispatch [:ui/set-panel-param :experiment-detail :ledger (not show-ledger)])}]
        [rc/input-text
         :class "mandate-project"
         :model (or assign "")
         :placeholder (str "assign project (now: " (or (:project item) "none") ")")
         :change-on-blur? false
         :on-change #(rf/dispatch [:ui/set-panel-param :experiment-detail :assign-project %])]
        [ui/toolbar-button
         {:label "Assign"
          :disabled? (or (= :pending act) (str/blank? (str assign)))
          :on-click #(rf/dispatch [:action/submit (str "exp-assign:" slug)
                                   {:method "PATCH"
                                    :url (str "/api/experiments/" (js/encodeURIComponent slug))
                                    :body {:project assign}
                                    :on-success [:machine/dispatch {:type :panel/load
                                                                    :panel :experiment-detail
                                                                    :url (str "/api/experiments/" slug)}]}])}]
        (cond
          (map? act) [:span.editor-err (:error act)]
          (= :done act) [:span.editor-ok "assigned"])]
       (if show-ledger
         [experiment-ledger slug]
         (cond
           (nil? payload) [ui/empty-box "Loading…"]
           (:error payload) [ui/error-box (:error payload)]
           :else [w/sections (domain/normalized-experiment-detail payload)]))])))

(defn experiments-page []
  (let [status @(rf/subscribe [:panel/ui-param :experiments :status ""])
        project @(rf/subscribe [:panel/ui-param :experiments :project ""])
        selected @(rf/subscribe [:panel/ui-param :experiments :slug nil])]
    [ui/page
     "experiments-view"
     [experiments-list status project selected]
     (when-not (str/blank? (str selected))
       ^{:key selected}
       [experiment-detail selected])]))
