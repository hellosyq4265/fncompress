import { createApp } from 'vue'
import { createRouter, createWebHashHistory } from 'vue-router'
import App from './App.vue'
import { pages } from './pages'

const router = createRouter({
  // Hash history: works inside the fnOS iframe without server-side rewrites.
  history: createWebHashHistory(),
  routes: [
    { path: '/', redirect: '/tasks' },
    ...pages.map((p) => ({ path: p.path, component: p.component, meta: { title: p.title } })),
    { path: '/:rest(.*)*', redirect: '/tasks' },
  ],
})

createApp(App).use(router).mount('#app')
