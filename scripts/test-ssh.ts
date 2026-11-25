// Optional Linux integration test: isolated unprivileged OpenSSH server.
// No user SSH keys/configuration or remote hosts are used or changed.
// Run with: bun scripts/test-ssh.ts
import {mkdtempSync, readFileSync, writeFileSync, rmSync} from 'node:fs';
import {spawn, spawnSync} from 'node:child_process';
import {tmpdir, userInfo} from 'node:os';
import {join} from 'node:path';
import {createServer} from 'node:net';

const dir = mkdtempSync(join(tmpdir(), 'disktree-ssh-test-'));
let server;
function run(cmd, args, env = process.env) {
  const result = spawnSync(cmd, args, {env, encoding:'utf8'});
  if (result.stdout) process.stdout.write(result.stdout);
  if (result.stderr) process.stderr.write(result.stderr);
  if (result.status !== 0) throw new Error(`${cmd} exited ${result.status}`);
}
try {
  run('cargo', ['xtask', 'portable']);
  const socket = createServer();
  await new Promise(resolve => socket.listen(0, '127.0.0.1', resolve));
  const port = socket.address().port;
  await new Promise(resolve => socket.close(resolve));
  for (const key of ['host', 'client']) {
    run('ssh-keygen', ['-q','-t','ed25519','-N','','-f',join(dir,key)]);
  }
  const config = join(dir, 'sshd_config');
  writeFileSync(config, `Port ${port}\nListenAddress 127.0.0.1\nHostKey ${dir}/host\nAuthorizedKeysFile ${dir}/client.pub\nStrictModes no\nUsePAM no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPidFile ${dir}/pid\nLogLevel VERBOSE\n`);
  const client = join(dir,'ssh_config');
  writeFileSync(join(dir,'known_hosts'), `[127.0.0.1]:${port} ${readFileSync(join(dir,'host.pub'),'utf8')}`);
  writeFileSync(client, `Host fixture\n HostName 127.0.0.1\n Port ${port}\n User ${userInfo().username}\n IdentityFile ${dir}/client\n IdentitiesOnly yes\n BatchMode yes\n UserKnownHostsFile ${dir}/known_hosts\n StrictHostKeyChecking yes\n ConnectTimeout 5\n`);
  server = spawn('/usr/bin/sshd', ['-D','-e','-f',config], {stdio:['ignore','ignore','pipe']});
  let log='';
  await new Promise((resolve,reject) => {
    const timer = setTimeout(()=>reject(new Error('sshd startup timed out: '+log)),5000);
    server.stderr.on('data', data => {
      log += data;
      if(log.includes('Server listening')) {clearTimeout(timer); resolve();}
    });
    server.on('exit',code=>{clearTimeout(timer); reject(new Error(`sshd exited ${code}: ${log}`));});
  });
  console.log(`Testing a real SSH connection to isolated sshd on localhost:${port}`);
  run('ssh',['-F',client,'fixture','true']);
  if (process.env.DISKTREE_PROFILE_ROOT) {
    const root = process.env.DISKTREE_PROFILE_ROOT;
    if (!root.startsWith(join(tmpdir(), 'disktree-profile-'))) throw new Error('Only disposable profile fixtures allowed');
    const binary = join(process.cwd(), 'target/release/examples/profile_remote');
    const helper = join(process.cwd(), `target/${process.arch === 'arm64' ? 'aarch64' : 'x86_64'}-unknown-linux-musl/release/disktree-cli`);
    for (const mode of ['ssh-plain', 'ssh']) {
      for (let trial=0; trial<5; trial++) run(binary, [mode,root,client,helper]);
    }
  } else run('cargo',['test','-p','disktree-core','real_ssh_transport_','--','--ignored','--nocapture'], {...process.env,DISKTREE_TEST_SSH_CONFIG:client,DISKTREE_TEST_HELPER:join(process.cwd(),`target/${process.arch === 'arm64' ? 'aarch64' : 'x86_64'}-unknown-linux-musl/release/disktree-cli`),DISKTREE_TEST_ARCHIVE:join(process.cwd(),'target/portable/disktree')});
  console.log(`${process.env.DISKTREE_PROFILE_ROOT ? 'SSH PROFILING COMPLETE' : 'REAL SSH SCAN PASSED'}. Temporary host key, client key and server will be removed.`);
} finally {
  if (server && server.exitCode === null) {
    server.kill('SIGTERM');
    await new Promise(resolve=>server.once('exit',resolve));
  }
  rmSync(dir, {recursive:true, force:true});
}
