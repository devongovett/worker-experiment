const binding = require('./');
const {Worker} = require('worker_threads');
const cpus = require('os').cpus().length;

async function start() {
  let workers = [];
  for (let i = 0; i < cpus / 2 - 1; i++) {
    let w = new Worker(__dirname + '/worker.js');
    workers.push(new Promise(resolve => w.once('message', resolve)));
  }
  await Promise.all(workers);
}

async function test() {
  await start();
  console.time();
  console.log(binding.doStuff(10000));
  console.timeEnd();
}

test();
