// Synchronous pre-stub for window.binkflixPlayer.
//
// Loaded before player.js (an ES module, which loads async). Queues any
// calls made during the gap; the module replays the queue once it's ready.
window.binkflixPlayer = {
    __queue: [],
    setAss: function () { this.__queue.push(["setAss", [...arguments]]); return Promise.resolve(); },
    setVtt: function () { this.__queue.push(["setVtt", [...arguments]]); return Promise.resolve(); },
    clear:  function () { this.__queue.push(["clear",  [...arguments]]); return Promise.resolve(); },
};
