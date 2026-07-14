(ns pgmcp.webui.views.common
  (:require [clojure.string :as str]
            [pgmcp.webui.schema :as schema]
            [re-com.core :as rc]))

(def truncation-marker "\n... truncated")

(defn bounded-text [limit value]
  (let [text (str (or value ""))
        limit (max 0 limit)]
    (if (<= (count text) limit)
      text
      (let [marker (if (> (count truncation-marker) limit)
                     (subs truncation-marker 0 limit)
                     truncation-marker)
            keep (max 0 (- limit (count marker)))]
        (str (subs text 0 keep) marker)))))

(defn preview-text [value]
  (bounded-text schema/max-preview-chars value))

(defn json-text [value]
  (.stringify js/JSON (clj->js value) nil 2))

(defn json-preview [value]
  (preview-text (json-text value)))

(defn panel [title payload]
  [:article.panel
   [:h2 title]
   [:pre (json-text payload)]])

(defn empty-box [text]
  [:div.empty text])

(defn error-box [message]
  [:div.error-box (or message "Request failed")])

(defn meta-row [& items]
  [:div.meta
   (for [[idx item] (map-indexed vector (remove str/blank? (map str items)))]
     ^{:key idx} [:span item])])

(defn summary-row [items]
  [:div.summary
   (for [[idx item] (map-indexed vector (remove str/blank? (map str items)))]
     ^{:key idx} [:span item])])

(defn toolbar-button
  "A toolbar button. :variant (:primary | :ghost | :danger) adds the matching
  btn-* class for emphasis; the default (nil) is the outline secondary style.
  :attr passes through to re-com (title / aria-*)."
  [{:keys [label active? on-click disabled? variant attr]}]
  [rc/button
   :label label
   :class (str "toolbar-button"
               (when active? " active")
               (when variant (str " btn-" (name variant))))
   :disabled? disabled?
   :attr (or attr {})
   :on-click on-click])

(defn labeled-field
  "A small uppercase label stacked above a control — for filter / query bars. A
  plain div (not a <label>) so it never overrides a wrapped button's accessible
  name / click target."
  [label control]
  [:div.field
   [:span.field-label label]
   control])

(defn skeleton-rows
  "n shimmer placeholder rows for a pane's loading state."
  ([] (skeleton-rows 6))
  ([n]
   (into [:div.skeleton-rows]
         (for [i (range n)]
           ^{:key i}
           [:div.skeleton.skeleton-row
            {:style {:width (str (- 100 (* 8 (mod i 4))) "%")}}]))))

(defn page [class & children]
  (into [:section {:class (str "view active " class)}] children))
