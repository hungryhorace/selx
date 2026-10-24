// Native test + release build on every release target. No cross-compilation:
// each stage runs on a node of the matching architecture. Only the amd64
// node has Docker, so any containerised stage must be pinned to 'amd64'.
pipeline {
    agent none
    options { timestamps(); buildDiscarder(logRotator(numToKeepStr: '20')) }
    stages {
        stage('Test + build') {
            matrix {
                axes {
                    axis { name 'NODE'; values 'amd64', 'aarch64', 'mac' }
                }
                agent { label "${NODE}" }
                stages {
                    stage('Test') {
                        steps {
                            // The justfile is the single definition of test/build;
                            // `just test` locally runs exactly this. Bootstrap just
                            // once per node if missing (installs to ~/.cargo/bin).
                            // Pinned: a just or cargo-deny release must not change
                            // what CI runs without a commit here saying so.
                            sh 'command -v just >/dev/null || cargo install just --locked --version 1.58.0'
                            sh 'command -v cargo-deny >/dev/null || cargo install cargo-deny --locked --version 0.20.2'
                            sh 'cargo --version && rustc --version && just --version'
                            sh 'just test'
                        }
                    }
                    stage('Build') {
                        steps {
                            sh 'just build'
                        }
                    }
                }
            }
        }
    }
}
